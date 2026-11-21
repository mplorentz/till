# ADR 0005 — Fill-channel security boundary: bounded residual now, camofox event redaction before real merchants

- **Status:** Accepted
- **Date:** 2026-11-21
- **Bead:** decision from the till-a0q spike findings (PR #3); encoded per
  overseer decision ("option E")
- **Spec:** §7 (Camofox interaction rules), §8.2 (interim trust assumptions)
- **Supersedes:** none — refines the spec's §7 "fill via evaluate, not /type"
  rule, which the spike proved *necessary but not sufficient*

## Context

The spike (docs/findings/camofox-spike.md) measured, against the live
Camofox instance:

1. `POST /tabs/:id/evaluate` publishes **both the expression (which would
   inline card values) and the result** on the plugin event bus
   (`pluginEvents.emit('tab:evaluate', {…, expression})`, server.js).
2. Tabs are strictly `userId`-scoped with no transfer API and `userId`
   carries no authentication, so the broker **must** share the agent's
   `userId` — and with it, the agent retains full DOM access to the tab
   throughout the fill→submit window (readback demonstrated, not
   theoretical).
3. Playwright tracing writes fill values to disk in plaintext (spike
   finding A).

Together: the event bus hands card values *passively* to anything
subscribed — but with the precision the spike added: `pluginEvents` is an
in-process Node `EventEmitter` inside the camofox container. **An
HTTP-only agent (Jamie) cannot subscribe to the bus at all** — the
spec's original “/type emits entered text … the agent can see it” was
too strong. The bus exposure is a *plugin supply-chain* channel (any
plugin loaded inside the container — including ones added later — is
handed card digits as a plain object; none subscribe today). The
**agent-side** residual is the DOM readback via the shared `userId`,
which is real, measured, and unaffected by any event redaction. This
is why the session lock (Decision 5) is the actual control and event
redaction is defense-in-depth.

## Prior art

- **camofox itself** already fixed the `/type` half of this exact problem:
  [jo-inc/camofox-browser#9436](https://github.com/jo-inc/camofox-browser/pull/9436)
  — "The text typed into a login form is a password, so any plugin
  subscribing to `tab:type` receives credentials it never asked for" —
  emitting `textLength` instead of `text` (merged in v1.16.0; the
  `tab:evaluate`/`tab:evaluated` pair was **not** covered and remains
  unredacted on current main).
- **browser-use** shipped the same class of leak and spent 2026 patching
  it: raw password to LLM
  ([#2226](https://github.com/browser-use/browser-use/issues/2226)), DOM
  snapshot redaction
  ([#4388](https://github.com/browser-use/browser-use/pull/4388)),
  length-not-value action logs
  ([#6455](https://github.com/browser-use/browser-use/pull/6455)) — all
  redaction at the source, never at the consumer.
- **Password managers** draw the boundary exactly where we do: 1Password's
  stated position is that "the security boundary of Autofill is the
  action of filling; after that, responsibility transfers to the page."
- **Playwright** maintains you cannot reliably scrub secrets after the
  fact ([microsoft/playwright#19992](https://github.com/microsoft/playwright/issues/19992))
  — redaction belongs at emit.

## Considered options

- **A. Patch camofox upstream** (redact the evaluate pair at emit) —
  structurally correct; couples the baseline to client-side deployment
  work.
- **B. Out-of-band fill values** (one-shot localhost fetch from inside the
  page) — keeps digits out of the expression, but is fragile against
  merchant CSPs (`connect-src`) and adds moving parts; no industry
  precedent.
- **C. Consumer-side suppression** (Jamie pauses event consumption) —
  adversary controls the consumer; not a trust boundary. Rejected.
- **D. Accept and bound** — treat the bus event as inside the §8.2
  residual window with mitigations.
- **E. D now, A before real merchants** — chosen.

## Decision

1. The baseline ships with the **bounded residual**: the fill-channel
   boundary is the broker's own egress plus the moment of fill; what is
   in the page after fill belongs to the page (the 1Password boundary).
   Residual exposure: one transient plugin event, inside a window where
   the agent already has direct DOM access.
2. **Mandatory mitigations** (enforced by the fill engine, bead till-ipx
   — binding regardless of which fill primitive is chosen):
   - exactly **one** fill operation per payment (one `evaluate` call, or
     one `/type` batch) — exactly one bus event;
   - nothing sensitive reaches any result/return channel;
   - Playwright **tracing off** for payment tabs (spike finding A);
   - `about:blank` containment via `evaluate` after terminal outcome
     (spec §7; `/navigate` is rejected for it — spike finding B).
3. **Fill primitive — `/type` on camofox ≥ v1.16, primary** (revised
   2026-11-21 per overseer: the initial implementation must not depend
   on any unmerged upstream PR). `#9436`'s redaction (`textLength`
   only on the `tab:type` bus payload) **ships in released v1.16.0** —
   choosing `/type` means the broker is bus-clean today with zero
   upstream dependencies, and its real keystrokes give higher input
   fidelity than a programmatic value set. `evaluate` is **demoted to
   the no-card-digit uses only**: `about:blank` containment (finding B:
   `/navigate` rejects it) and clearing/masking fields before the
   result screenshot — those expressions contain field *references*,
   never values, so they are safe on any version. Falling back to
   `evaluate` for a fill is disallowed unless the Tier-1 redaction has
   landed on that host; pre-1.16 hosts (where both primitives emit raw
   content) are out of scope entirely.
   **Prerequisite, not a runtime gate** (2026-11-21 overseer decision):
   camofox ≥ v1.16 is *assumed* — modern hosts only. Documented in the
   spec and deployment notes, not enforced by the broker at runtime.
4. **Tier 1 — stopgap hardening** (before real-merchant testing, tracked
   as bead till-aal): deploy camofox ≥ v1.16 on every host the broker
   runs against and set `CAMOFOX_ACCESS_KEY` (gets #9436's `/type`
   redaction and #3087's access-key middleware); file an upstream PR to
   jo-inc/camofox-browser mirroring #9436 for the `tab:evaluate`/
   `tab:evaluated` pair (`expressionLength` + correlation hash; result
   payload = type only).
5. **Tier 2 — the real control**: broker-exclusive session access in
   camofox — per-principal auth (the broker gets its own credential,
   not a shared string `userId`) plus a **tab lease/transfer API**: the
   agent hands the tab to the broker for the fill window, agent calls
   are refused during the lease, the broker releases to `about:blank`.
   This structurally kills the DOM-readback residual (which no event
   redaction touches) and the bus exposure together. It is the spec's
   deferred non-goal made concrete, it is upstream work in the client's
   infra, and it is the position to propose in the client conversation:
   event redaction is the days-scale stopgap; the lease is the fix.

## Consequences

- The mock-site phase (all baseline testing) is unaffected: the bus leak
  harms nothing against a site we control.
- Card digits still never appear in: the model context, API responses,
  logs, the Hermes plugin, or Discord (unchanged §7 rules).
- The structural guarantee ("never on the bus at all") arrives with the
  upstream patch, before any real-merchant traffic.
- `docs/SPEC.md` §8.2 is amended to record the measured facts and this
  boundary.
