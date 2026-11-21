# ADR 0001 — Persist broker state in SQLite via rusqlite (WAL)

- **Status:** Accepted
- **Date:** 2026-11-14
- **Bead:** till-a0q (spike) → applies to till-1gt
- **Spec:** §3 D12/D14, §4 state machine, §7 submit-once, US-12/US-13

## Context

The broker is the sole owner of payment state. Several of its obligations
are *durability* obligations, not request-scoped ones:

* **The submit-once latch must survive a crash.** Spec §7 and US-12: if
  the broker loses contact around the submit click, the payment goes
  `ambiguous`, never "try again". That is only sound if "I already sent
  the click" is recorded on disk *before* the click is attempted.
* **Crash recovery is a state transition.** A payment found in
  `submitting` at startup becomes `ambiguous` (US-12). This requires the
  state to be readable at boot with no in-memory history.
* **Idempotency** (US-13): `POST /payments` dedupes on
  `idempotency_key` across restarts.
* **Debug sessions are part of the workflow.** The repo's operating
  stance is that an operator can inspect broker state directly during an
  incident.

Constraints: single-host Docker sidecar on the test host; low write
volume (tens of payments, not thousands); no external database to
operate; `unsafe_code = "forbid"` workspace-wide; no new dependency
without justification (AGENTS.md rule 5).

The Camofox spike (till-a0q, `docs/findings/camofox-spike.md`) added a
timing dimension: Camofox reaps an idle tab after 5 minutes
(`TAB_INACTIVITY_MS`) and closes an idle session after 10 minutes
(`SESSION_TIMEOUT_MS`). Whatever the broker persists must therefore be
enough to *reconstruct* a payment whose browser tab no longer exists —
the tab is not the record.

## Decision

Persist all payment state in **SQLite via `rusqlite`**, in **WAL mode**
(`journal_mode=WAL`, `synchronous=FULL` for state-changing writes), one
database file on a mounted volume.

* One `payments` row per payment: `payment_id`, `idempotency_key`
  (unique), `state`, `cart_json`, `tab_id`, `session_key`,
  `fields_json` (selectors + `field_kind` **only — never values**),
  `submit_selector`, `created_at`, `updated_at`, `approval_deadline`,
  `terminal_reason`.
* One `transitions` append-only table: `(payment_id, from_state,
  to_state, at, reason)` — the state machine is the product, so its
  history is queryable.
* The **submit-once latch is a row written and committed before the
  `click` request is issued** (`submitted_at`, `submit_attempted=1`).
  Recovery rule: any row with `submit_attempted=1` and a non-terminal
  state at startup ⇒ `ambiguous`.
* Card ciphertext lives in a separate table (`card_blobs`) keyed by
  payment/owner; see ADR 0002.

`rusqlite` is used with the `bundled` feature so the container needs no
system libsqlite.

## Consequences

**Good**

* Crash-consistent submit-once latch with a single `COMMIT`; no separate
  WAL-of-a-WAL.
* Recovery is a plain `SELECT … WHERE state='submitting'` at boot.
* Operator-friendly: `sqlite3 /data/till.db '.tables'` in a debug
  session, which is exactly why D14 chose SQLite over redb/sled.
* Idempotency is a `UNIQUE` index — the database, not application code,
  enforces it (a duplicate insert is a 409, not a race).
* No server process to operate; the file is the state.
* Works with the existing "single writer" shape: one broker process.

**Bad / accepted costs**

* **WAL does not make the network call atomic.** A crash between
  `COMMIT(submit_attempted)` and the `click` produces `ambiguous` even
  though nothing may have been submitted. That asymmetry is deliberate:
  ambiguous is recoverable by a human, a double charge is not.
* Single-writer concurrency: long-poll waiters must not hold the write
  connection. Reads for `GET /payments/:id` go through a separate
  read-only connection pool; writes are serialized behind a mutex.
* No built-in cross-process fan-out — the notification path is in ADR
  0004 (`watch`/`Notify`), not the database.
* `synchronous=FULL` costs an fsync per transition. With tens of
  payments per day that is irrelevant; it buys the durability the
  latch depends on.
* Schema migrations are hand-rolled (`user_version` pragma). Acceptable
  at this size; revisit if the schema churns.

## Alternatives considered

* **redb / sled** — pure-Rust, embeddable, no C dependency. Rejected on
  D14's inspectability argument: during an incident the operator wants
  to read the state machine's history with `sqlite3`, not with a bespoke
  dumper.
* **JSON file + atomic rename** — simplest, but no transactional
  multi-row write, no unique index for idempotency, and crash safety
  depends on hand-rolled fsync ordering at exactly the point (submit)
  where it matters most.
* **Postgres in a second container** — real transactions and
  concurrency, but a new service, a new credential, a new backup story,
  and a network hop on the critical path, for a workload of tens of
  rows.

## References

* `docs/SPEC.md` §3 (D12, D14), §4, §7, US-12, US-13
* `docs/findings/camofox-spike.md` — "Idle reaping bounds the approval
  window" (why the tab cannot be treated as the record)
* ADR 0002 (what is stored in `card_blobs`), ADR 0004 (how waiters are
  woken)