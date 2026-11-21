# ADR 0004 — Long-poll `wait_for_payment` with axum + `tokio::sync::watch`/`Notify`

- **Status:** Accepted
- **Date:** 2026-11-14
- **Bead:** till-a0q (spike) → applies to till-1gt
- **Spec:** §3 D7, D8, D11; §5 (`wait_for_payment`), US-2, US-4

## Context

The agent's turn must stay alive across polls without burning LLM tokens
on a sleep loop (D7), and the broker is forbidden from pushing into the
agent's thread for the baseline (D7, §11 non-goals). So the outcome
travels by **bounded long-poll**: `wait_for_payment(payment_id,
timeout_seconds)` blocks until the state changes or the timeout elapses,
and non-terminal returns let the agent re-invoke cheaply.

Constraints and measured facts:

* Approval can take minutes (a human clicks), so a naive
  request-holds-a-thread-per-waiter design is wasteful but bounded:
  one payment, one agent, one waiter in practice.
* Camofox reaps an **idle tab after 5 minutes** (`TAB_INACTIVITY_MS`),
  and closes an idle **session after 10 minutes**
  (`SESSION_TIMEOUT_MS`), both default and unset in the current
  deployment (spike findings E). A long-poll cannot make the browser
  live longer; the approval TTL must be reconciled with those numbers,
  and the broker must be able to answer a waiter with
  `approval_timeout`/`aborted_before_submit` when the tab is gone.
* Submit costs ~2 s and is the point of no return; the wait endpoint
  must never be the thing that decides to retry.
* `axum` + `tokio` are already the HTTP stack (D12).

## Decision

> **Erratum (2026-11-22, bead till-1gt).** The endpoint is
> `GET /payments/{id}/wait?timeout=` — the converged design for the broker
> core chose `GET` (the wait has no body and no side effect), while this ADR
> as originally written said `POST /payments/:id/wait`. Everything else below
> is unchanged and is implemented as written. The plugin tool remains
> `wait_for_payment(payment_id, timeout_seconds)`.

`POST /payments/:id/wait` (and the plugin's `wait_for_payment`) is an
**axum handler that subscribes to an in-process broadcast of state
changes for that payment and awaits it with a bounded timeout**.

* One `tokio::sync::watch::Sender<State>` per in-flight payment, held in
  a registry keyed by `payment_id`. Waiters clone the `Receiver`; the
  state machine publishes on every transition.
* `tokio::sync::Notify` for "something changed, re-check" wakeups where
  a value is not needed (e.g. multiple waiters on the same payment).
* The handler loop is authoritative-by-DB, not by the channel: on wake,
  re-read the row (ADR 0001); the channel is only a latency
  optimisation. This keeps behaviour correct if a notification is
  missed (e.g. transition performed by a different task).
* Bounded by `Timeout` with the caller's `timeout_seconds`, clamped to a
  server maximum. On timeout: return the **current** non-terminal state
  (`pending_approval`, `approved`, `filling`, `submitting`,
  `awaiting_result_review`) with HTTP 200 — not an error. The agent
  re-invokes.
* `GET /payments/:id` is the non-blocking read (US recovery path, D11).
* **Approval TTL must be ≤ the tab-inactivity window** (default 5 min)
  unless the broker deliberately refreshes the tab, and the configured
  TTL is exported in the approval message (§6 "TTL display"). The
  registry entry is dropped on terminal state; a waiter for a payment
  whose tab/session vanished resolves via the state machine to
  `aborted_before_submit` (pre-submit) or `ambiguous` (post-submit).

## Consequences

**Good**

* No polling loop in the agent, no tokens burned, no broker-initiated
  callback (stays inside D7's baseline).
* Waiters are cheap: a `watch` receiver is a few words, and hundreds of
  spurious wakeups cost nothing.
* Losing a notification is harmless because the DB is re-read on every
  wake — the design tolerates the race between "publish" and "commit".
* The bounded timeout means an abandoned agent turn cannot pin a task
  forever, and the returned non-terminal state lets the agent decide
  whether to keep waiting.
* `watch` gives the latest value (state), which is exactly the
  semantics needed: an agent that missed intermediate states does not
  care about `filling`.

**Bad / accepted costs**

* Single-process only: the registry does not survive a restart, so a
  waiter is dropped on deploy/crash and must re-poll `GET /payments/:id`
  (D11 already says polling retrieves broker state; this is consistent
  but must be documented for the agent skill).
* Two sources of truth for "has it changed" (channel + DB row). They are
  reconciled by always trusting the DB; the channel is advisory.
* Long-polls hold a connection. With one agent this is trivial; if many
  agents share a broker, an FD/concurrency cap is needed (note for
  later, not a baseline problem).
* The 5-minute tab-inactivity ceiling is a *product* constraint, not an
  implementation detail: a human who is slow to click loses the tab.
  Choosing a TTL below it, or refreshing the tab, is a decision this
  ADR forces to be explicit.
* No broker-initiated push means the agent must still be alive to
  collect the outcome; a restarted agent needs `get_payment`.

## Alternatives considered

* **Polling with `GET /payments/:id` only** — simplest and fully
  recoverable, but the spec's whole reason for D7 is to avoid the agent
  burning turns on sleeps; polling in the agent is what D7 rejects.
* **Server-Sent Events / WebSocket push** — pushes *into the agent's
  channel*, which is closer to the deferred callback pattern (§11) and
  needs connection-lifecycle handling; hurts nothing to defer.
* **Broker-initiated Discord message into the agent's thread** —
  explicitly a non-goal for the baseline (§11, D7).
* **Holding the HTTP request open for the entire payment** (no bounded
  timeout) — turns a browser session timeout into a hung agent turn, and
  gives no chance to reconcile the 5-minute tab reap.
* **`tokio::sync::broadcast`** — right for many subscribers of a stream
  of events, wrong here: it lags and drops, and we only want the latest
  state. `watch` is the fit; `Notify` covers the "just re-check" case.

## References

* `docs/SPEC.md` §3 D7/D8/D11, §5, §6, §11, US-2, US-4
* `docs/findings/camofox-spike.md` — finding E (idle reaping bounds the
  approval window), Q5 (submit ≈ 2 s, no rate limiting)
* ADR 0001 (DB is the authority), ADR 0003 (Approve click drives the
  transitions this endpoint awaits)