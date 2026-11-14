# till

**till** *(n.)* — the cash register drawer. The money sits in the drawer
while the sale is still in progress; when the sale is confirmed, the
drawer closes.

A payment broker sidecar for agentic checkout flows: it stores card
details encrypted at rest, obtains human approval over Discord, fills the
card fields into a [Camofox](https://github.com/redf0x1/camofox-browser)
browser tab, and submits the checkout — without card digits ever entering
the agent's model context, the event bus, or any log.

```
Hermes agent ── request_payment / wait_for_payment / get_payment ──▶ till
                                                                     │
                                              ┌──────────────────────┴─────┐
                                              ▼                            ▼
                                    Camofox (fill, submit,         Discord (human
                                    screenshot, about:blank)       approve / classify)
```

- Canonical spec: [`docs/SPEC.md`](docs/SPEC.md)
- Agent instructions: [`AGENTS.md`](AGENTS.md)

## Layout

| Path | What it is |
|---|---|
| `crates/broker` | axum HTTP server, payment state machine, storage, crypto |
| `crates/camofox-client` | typed client for the Camofox REST API |
| `crates/discord-gw` | serenity bot: approval + result-review interactions |
| `crates/mock-checkout` | local 4-mode test checkout site + `/stats` counter |
| `plugin/` | Jamie-side Hermes plugin (Python) — thin HTTP client |
| `docker/` | images + compose for the test-host deployment |

## Development

```bash
./script/checks        # fmt + clippy + tests (what CI runs)
cargo test --workspace
```

License: AGPL-3.0-only.
