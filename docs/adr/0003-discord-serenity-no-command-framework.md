# ADR 0003 — Discord gateway on serenity, no command framework

- **Status:** Accepted
- **Date:** 2026-11-14
- **Bead:** till-a0q (spike) → applies to till-6
- **Spec:** §3 D1/D13, §6, §7; US-9, US-10, US-15, US-16

## Context

Approval is the trust boundary of the whole product: the human is
directly in a signed Discord interaction with the broker, and the agent
is deliberately outside it (§1, D1). That makes the Discord gateway a
security component, not a notification helper. Its obligations:

* Post an approval message: structured cart + broker-captured screenshot
  + **Approve/Decline** + TTL (US-6, US-15). The screenshot must have
  payment fields cleared/masked first (US-10).
* Post a result-review message: post-submit screenshot + **Order
  confirmed / Failed — no order or charge / Outcome unclear**
  (US-10, US-16).
* Reject clicks on terminal payments and any late Approve after
  `approval_timeout` — with **nothing filled or submitted** (US-9, §6
  "Stale-interaction handling").
* Enforce an approver allowlist (open question §12.2; D4 per-agent
  identity).
* Never let a card digit into a message (US-14, AGENTS.md rule 1).

Constraints: Rust (D12); the bot only sends component messages and
handles interactions, so a command framework is dead weight (D13); a
single bot token in a container secret; buttons must be attributable to
a specific `payment_id` across restarts.

## Decision

Use **serenity** (bead pins 0.13.x; pin the exact release when the
dependency is added — the version was not re-verified in the spike host,
which has no crates.io access) **without a command framework**.

* Gateway-only client: `EventHandler` for `interaction_create`; no
  `framework`, no prefix commands, no regex routing.
* **`payment_id` travels in the component `custom_id`**, e.g.
  `till:approve:<payment_id>`, `till:decline:<payment_id>`,
  `till:result:<payment_id>:succeeded|failed|ambiguous`. The broker is
  the authority on what a click may do; the button carries no policy.
* Every interaction handler: (1) check the clicker against the allowlist,
  (2) look the payment up in SQLite (ADR 0001), (3) apply the state
  transition **in the database first**, (4) only then act (fill/submit)
  and `respond_ephemeral` — or reply "this request has expired" for
  terminal/late clicks.
* Stale/late clicks are rejected by state, not by message age: a
  `pending_approval` → `approval_timeout`/`declined`/terminal row means
  the Approve is refused and no Camofox call is made.
* Screenshots are captured by the broker, masked/cleared in the DOM
  before capture, and uploaded as Discord attachments. Card values are
  never rendered into embed text.
* Allowlist is a config list of Discord user IDs; unknown clicker ⇒
  ephemeral refusal + log (user id only, never card data).

## Consequences

**Good**

* The interaction path is a state machine dispatch, not a command
  parser: one place where "may this click do anything?" is decided.
* Buttons survive broker restarts because `custom_id` is just data and
  the authority is the DB row.
* Late Approve after `approval_timeout` is structurally inert: the row
  is terminal, so the handler cannot reach the fill call. This satisfies
  US-9 without a timer racing the click.
* No framework ⇒ no accidental public slash commands; the bot cannot be
  invoked from anywhere except the components it posted.
* Serenity is the mature Rust Discord crate and D13 already chose it;
  using it framework-free keeps the dependency's surface (and its
  attack surface) small.

**Bad / accepted costs**

* Security-critical logic sits in a `match` on `custom_id` — needs a
  test per state × button, which is exactly the "encode the decision as
  a state transition with a test" rule in AGENTS.md.
* Serenity pulls a heavy dependency tree (tokio, TLS, gateway
  reconnects). Accepted: there is no smaller maintained option that
  gives signed interactions.
* Discord rate limits and interaction 3-second ACK windows are real:
  fill+submit (~2 s per the spike) plus screenshot capture is close to
  the ACK budget, so the handler must `defer`/`respond` promptly and do
  the Camofox work with an explicit follow-up edit.
* A single bot token identifies the broker to Discord; token compromise
  is equivalent to the broker itself. Treated as a container secret and
  rotated on suspicion.
* Buttons in old messages remain clickable forever; staleness is handled
  by state, which means every handler must read the DB before acting.
  (That is the point, but it is a mandatory discipline, not a free win.)

## Alternatives considered

* **serenity with its command framework** — brings slash-command
  registration, argument parsing and a macro DSL we never use, and makes
  it easy to accidentally expose a command surface. Rejected by D13.
* **Raw Discord HTTP + webhooks (no gateway)** — components require
  interactions, which require the gateway (or a webhook endpoint with
  signature verification). More hand-rolled crypto/HTTP for no gain over
  serenity.
* **twilight** — a fine, more modular ecosystem, but D13 already chose
  serenity and the team's examples/idioms are serenity-shaped; the
  gateway logic here is small either way.
* **DM the owner instead of channel messages** — loses the auditable
  approval thread and the screenshot review UX of §6.

## References

* `docs/SPEC.md` §3 D1/D13, §6, §7, US-9/US-10/US-15/US-16
* ADR 0001 (state is the authority for a click), ADR 0002 (nothing to
  leak into a message), ADR 0004 (how the payment proceeds after
  Approve)