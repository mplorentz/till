# till — Agent Instructions

> **What this repo is:** a payment broker sidecar (Rust) that stores card
> details encrypted at rest and fills them into a Camofox browser tab after
> a human approves over Discord. Card digits must never enter an agent's
> model context, the Camofox plugin event bus, any log, or any Discord
> message. The full design is in `docs/SPEC.md` — read it before touching
> the fill path.

## Non-negotiable security rules

These follow from the spec (§7, US-14) and override everything else:

1. **Never log, trace, or serialize card data.** No PAN, expiry, or CVV in
   `tracing` calls, error messages, debug output, test output, or panic
   messages. Use the `secrecy`/`zeroize` wrappers; if a value is a card
   field, treat it as radioactive.
2. **Fill via `POST /tabs/:id/evaluate`, never `POST /tabs/:id/type`.**
   `/type` emits entered text onto the Camofox plugin event bus — the
   agent can see it. This is spec §7.
3. **Click submit exactly once per payment.** The submit-once latch is
   persisted *before* the click; if the broker loses contact with Camofox
   around the click, the payment goes `ambiguous`, never "try again".
4. **Mask or clear payment fields before every screenshot** that leaves
   the broker (spec US-10).
5. **No new dependencies without justification** — especially anything
   that touches card data, network egress, or the storage path.

## Repository layout

- `crates/broker` — axum HTTP API (`POST /payments`, long-poll wait),
  the payment state machine, rusqlite (WAL) persistence, age-encrypted
  card blobs.
- `crates/camofox-client` — typed client for the Camofox REST API
  (`http://localhost:9377` by default; per-userId tabs).
- `crates/discord-gw` — serenity bot: approval buttons, result-review
  buttons, approver allowlist, stale-interaction rejection.
- `crates/mock-checkout` — the 4-mode test site (`success`, `declined`,
  `ambiguous`, `short_hold`) + `/stats` submission counter.
- `plugin/` — the Hermes-side plugin (Python, `plugin.yaml` +
  `register(ctx)`), thin authenticated HTTP client for the broker API.
- `docker/` — deployment on the Mac Mini.

Integration boundaries (Camofox, Discord) sit behind traits in the broker
so the state machine is testable without either.

## Commands

```bash
./script/checks          # what CI runs: fmt --check, clippy -D warnings, tests
cargo test --workspace
cargo run -p mock-checkout
```

## Working rules

- Branch + PR for all work; never commit to `main`.
- The state machine is the product. When in doubt, encode the decision as
  a state transition with a test, not as a comment.
- Bead-driven work: run titles are prefixed `<bead-id>: <title>`.

## Beads reference

This project uses bd (beads) for task tracking — prefix `till`, database
`till` on the shared Dolt server. Run `bd prime` for workflow context;
`bd ready` for available work; claim with `bd update <id> --claim` before
starting. The assignee is the mutex.
