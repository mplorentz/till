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

<!-- BEGIN BEADS INTEGRATION v:1 profile:minimal hash:970c3bf2 -->
## Beads Issue Tracker

This project uses **bd (beads)** for issue tracking. Run `bd prime` to see full workflow context and commands.

### Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work
bd close <id>         # Complete work
```

### Rules

- Use `bd` for ALL task tracking — do NOT use TodoWrite, TaskCreate, or markdown TODO lists
- Run `bd prime` for detailed command reference and session close protocol
- Use `bd remember` for persistent knowledge — do NOT use MEMORY.md files

**Architecture in one line:** issues live in a local Dolt DB; sync uses `refs/dolt/data` on your git remote; `.beads/issues.jsonl` is a passive export. See https://github.com/gastownhall/beads/blob/main/docs/SYNC_CONCEPTS.md for details and anti-patterns.

## Agent Context Profiles

The managed Beads block is task-tracking guidance, not permission to override repository, user, or orchestrator instructions.

- **Conservative (default)**: Use `bd` for task tracking. Do not run git commits, git pushes, or Dolt remote sync unless explicitly asked. At handoff, report changed files, validation, and suggested next commands.
- **Minimal**: Keep tool instruction files as pointers to `bd prime`; use the same conservative git policy unless active instructions say otherwise.
- **Team-maintainer**: Only when the repository explicitly opts in, agents may close beads, run quality gates, commit, and push as part of session close. A current "do not commit" or "do not push" instruction still wins.

## Session Completion

This protocol applies when ending a Beads implementation workflow. It is subordinate to explicit user, repository, and orchestrator instructions.

1. **File issues for remaining work** - Create beads for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **Handle git/sync by active profile**:
   ```bash
   # Conservative/minimal/default: report status and proposed commands; wait for approval.
   git status

   # Team-maintainer opt-in only, unless current instructions forbid it:
   git pull --rebase
   bd dolt push
   git push
   git status
   ```
5. **Hand off** - Summarize changes, validation, issue status, and any blocked sync/commit/push step

**Critical rules:**
- Explicit user or orchestrator instructions override this Beads block.
- Do not commit or push without clear authority from the active profile or the current user request.
- If a required sync or push is blocked, stop and report the exact command and error.
<!-- END BEADS INTEGRATION -->

<!-- BEGIN BEADS CODEX SETUP: generated by bd setup codex -->
## Beads Issue Tracker

Use Beads (`bd`) for durable task tracking in repositories that include it. Use the `beads` skill at `.agents/skills/beads/SKILL.md` (project install) or `~/.agents/skills/beads/SKILL.md` (global install) for Beads workflow guidance, then use the `bd` CLI for issue operations.

### Quick Reference

```bash
bd ready                # Find available work
bd show <id>            # View issue details
bd update <id> --claim  # Claim work
bd close <id>           # Complete work
bd prime                # Refresh Beads context
```

### Rules

- Use `bd` for all task tracking; do not create markdown TODO lists.
- Run `bd prime` when Beads context is missing or stale. Codex 0.129.0+ can load Beads context automatically through native hooks; use `/hooks` to inspect or toggle them.
- Keep persistent project memory in Beads via `bd remember`; do not create ad hoc memory files.

**Architecture in one line:** issues live in a local Dolt DB; sync uses `refs/dolt/data` on your git remote; `.beads/issues.jsonl` is a passive export. See https://github.com/gastownhall/beads/blob/main/docs/SYNC_CONCEPTS.md for details and anti-patterns.
<!-- END BEADS CODEX SETUP -->
