# ADR 0008 — Camofox port extension and the masked-screenshot artifact handoff

- **Status:** Accepted
- **Date:** 2026-11-22
- **Bead:** till-ipx (Camofox client + fill/submit/screenshot engine)
- **Spec:** §6 (result-review UX), §7 (Camofox interaction rules); AGENTS.md
  rules 2, 3, 4, 5, 6, 7
- **Amends:** ADR 0005 (Decision 2.1, "one fill operation" — see §Decision 5);
  ADR 0001 (a new artifact directory beside the database, not a schema change)
- **Supersedes:** none

## Context

The converged design for till-ipx says the fill→submit→screenshot sequence is
**one supervised task**: `approved → filling → persist the submit latch →
submitting (single click) → page settle → clear/mask the payment fields →
awaiting_result_review (screenshot)`, with `about:blank` containment once the
payment settles. The state machine that drives it already exists
(`crates/broker/src/engine.rs`, `state.rs`), and the outside world sits behind
the `CamofoxPort` trait so the machine is testable without a browser.

Implementing the production side (a new `crates/camofox-client`) forced four
decisions that the port trait's original two methods (`fill_fields`,
`click_submit`) could not express:

1. **Where the sequencing responsibility sits.** The engine owns *order*
   (it is the product; AGENTS.md "Working rules"), the client owns
   *mechanics*. But settle → mask → screenshot needs port surface.
2. **How the result screenshot reaches Discord.** `DiscordPort` receives no
   pixels today, and `ResultReviewRequest` is a `Copy` struct of ids and the
   cart. The gateway (till-q1k) must be able to upload a masked PNG keyed by
   payment id.
3. **Which HTTP client to depend on** (AGENTS.md rule 5).
4. **What "exactly one fill operation" means** when the measured `/type`
   endpoint takes a single field (ADR 0005 Decision 2.1 says "one `/type`
   batch", which does not exist in the API).

## Considered options

- **A. Return the PNG through `DiscordPort`** (add a screenshot field to
  `ResultReviewRequest`). Rejected: it widens a `Copy` struct the gateway bead
  is already extending, and it makes the engine transport pixels through a
  trait that never needs to understand them.
- **B. Store the artifact in the broker's directory, keyed by payment id**
  (chosen). The gateway asks for a path.
- **C. Keep a screenshot byte array in the payments row.** Rejected: a BLOB of
  page pixels in the durable state machine is not state, and it outlives any
  need for it.
- **D. Hand-roll HTTP/1.1 over `tokio::net::TcpStream`.** Rejected: ~100 lines
  of framing and header parsing for one internal endpoint, with no audit
  trail, to avoid a dependency the workspace already trusts transitively
  (serenity/axum pull hyper; `reqwest` is the maintained client on it).

## Decision

1. **`CamofoxPort` widens to the supervised sequence.** Three methods are
   added beside `fill_fields` / `click_submit`:
   `wait_for_settle`, `capture_masked_result`, `contain_tab`.
   - `capture_masked_result` **returns pixels and clears the fields first**, in
     one method. Fusing mask+capture makes AGENTS.md rule 4 ("mask or clear
     payment fields before every screenshot that leaves the broker") a property
     of the only pixel-returning call path rather than a caller obligation.
   - `contain_tab` is `about:blank` via `evaluate`, because `POST /navigate`
     rejects it (spike finding B).
   - `wait_for_settle` exists because `/wait` is unusable near a submit button:
     it ignores its selector and can click consent dialogs (spike finding C).
     Settle detection is a bounded, digit-free `evaluate` poll from the broker
     side.

2. **The engine runs those steps after the confirmed click, best-effort.**
   The click is already proven once at that point, so a settle/mask/capture
   failure can never make the payment `ambiguous`; it is logged (payload-free,
   like every other port failure) and the payment stays
   `awaiting_result_review`. Containment is fire-and-forget on every terminal
   shape the broker can still reach: a pre-submit abort, an unconfirmed click,
   the human's outcome click, and crash recovery.

3. **Masked screenshots are written to a broker-managed artifact directory,
   one file per `(payment_id, kind)`.** `ScreenshotStore` writes
   `<dir>/<payment_id>-<kind>.png` atomically (temp file + rename), owner-only
   (`0600`). Two kinds exist: `approval` (US-6, before any card value is typed)
   and `result` (US-10, after masking). The Discord gateway (till-q1k) reads the
   file by path — this is the whole handoff contract between the crates.

4. **The HTTP client is `reqwest` 0.12 with `default-features = false` and the
   `json` feature.** TLS is deliberately *not* in the tree: camofox is reached
   over plain HTTP on the docker bridge (spike Environment). `reqwest` never
   logs a request body, and the only request that carries card values is the
   `/type` fill call, which is crate-private to the fill engine.

5. **"One fill operation" = one supervised pass, one `/type` per field, never
   retried.** The measured API has no batch form (spike Q1:
   `{userId, selector, text}`), so the ADR 0005 Decision 2.1 phrase "one
   `/type` batch" is read as one logical operation. On camofox ≥ 1.16 every
   `tab:type` event carries a length rather than the text, so N length-only
   events are still bus-clean. ADR 0005 Decision 2 carries an amendment note
   saying exactly this.

6. **No runtime version gate.** camofox ≥ 1.16 is a documented deployment
   prerequisite (SPEC §7, ADR 0005; ops keeps the host current), not something
   the broker probes or refuses (overseer correction, 2026-11-21).

## Consequences

- The engine gained one constructor parameter (`Arc<ScreenshotStore>`) and the
  port gained three methods; every test double implements them: the broker's
  drivers record the calls, which is how "settle → mask → capture, in that
  order" and "containment exactly once per settled payment" are asserted.
- Artifacts accumulate one small file per payment per kind and are overwritten
  in place, never appended. Retention/GC is deliberately out of scope here; the
  growth is bounded by payment count and the files are owner-only.
- The gateway can now upload a masked screenshot without either crate learning
  about Discord attachments: it needs a payment id and a directory.
- The accepted residual of ADR 0005 is unchanged: the agent can still read the
  tab during the fill→submit window (shared `userId`), which only upstream
  per-principal auth plus tab leasing fixes. `about:blank` containment bounds
  it at the end, as the spec requires.
- Card digits still never reach the model context, an API response, a log line,
  the Hermes plugin, or Discord: the fill is the only egress, masking precedes
  capture, and both are enforced by type shape rather than review.
