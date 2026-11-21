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

Together: the event bus hands the agent card values *passively* — but
only inside a window in which the same agent can already read them
*actively* from the DOM. The bus grants **no new capability**; it changes
passive capture from impossible to possible within that window.

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
   the subscriber already has direct DOM access.
2. **Mandatory mitigations** (enforced by the fill engine, bead till-ipx):
   - exactly **one** `evaluate` call per fill — all fields in a single
     expression, so exactly one bus event per payment;
   - the expression returns `undefined` — nothing in the result event;
   - Playwright **tracing off** for payment tabs (spike finding A);
   - `about:blank` containment via `evaluate` after terminal outcome
     (spec §7; `/navigate` is rejected for it — spike finding B).
3. **Hardening deliverable, gated before real-merchant testing**: an
   upstream PR to jo-inc/camofox-browser mirroring #9436 for the
   `tab:evaluate`/`tab:evaluated` pair (emit `expressionLength` plus a
   correlation hash; result payload carries only its type). Tracked as
   its own bead; part of the client conversation.
4. **Client deployment recommendation** (independent of this broker):
   upgrade the production camofox to ≥ v1.16.0 (gets #9436's `/type`
   redaction and the #3087 access-key middleware) and set
   `CAMOFOX_ACCESS_KEY`. The measurement host for this project runs
   v1.13.0, which predates both.

## Consequences

- The mock-site phase (all baseline testing) is unaffected: the bus leak
  harms nothing against a site we control.
- Card digits still never appear in: the model context, API responses,
  logs, the Hermes plugin, or Discord (unchanged §7 rules).
- The structural guarantee ("never on the bus at all") arrives with the
  upstream patch, before any real-merchant traffic.
- `docs/SPEC.md` §8.2 is amended to record the measured facts and this
  boundary.
