# ADR 0006 — Agent API keys and how human decisions reach the engine

- **Status:** Accepted
- **Date:** 2026-11-22
- **Bead:** till-1gt (broker core)
- **Spec:** §2, §5, §6; AGENTS.md rules 6–7

## Context

Spec §2 places three principals around the broker and never says how two of
the boundaries are implemented:

```
Hermes agent  ──HTTP──▶  Payment broker  ◀──button clicks──  Discord gateway
```

Two questions had to be answered by something in till-1gt, because the
broker core is where both contracts are defined:

1. **How does an agent authenticate?** D4 says "per-agent API keys", and
   US-13's idempotency is per agent. Nothing says what a key looks like on
   the wire, where it lives, or what a request without one does.
2. **How does a human's Approve / Decline / outcome click get into the state
   machine?** The gateway is a separate crate (`crates/discord-gw`, till-6),
   and its clicks must reach functions that perform transitions. Nothing
   says whether that is an HTTP call, a shared database write, or a library
   call.

Both are contract-shape choices with a security consequence: the approval
click is the *only* thing that authorizes card data to move (AGENTS.md rule
7), so wherever clicks are accepted is on the trust boundary. Leaving them
implicit risked till-6 and till-7 inventing incompatible answers.

## Decision

**Agent authentication**

* `Authorization: Bearer <key>`, one key per agent, keyed set configured via
  `TILL_AGENT_KEYS=agent_id=key[,agent_id=key]`.
* The broker derives the agent id from the presented key; nothing in the
  request body may claim an identity.
* Comparison is constant-time over every configured key (no early exit), so
  the answer does not depend on entry position. Key length is not hidden —
  keys are generated, not user-chosen.
* **Fail closed:** an unset or empty key set authenticates nobody. A
  misconfigured broker is unavailable, never open.
* A payment is scoped to the agent that created it: reading another agent's
  payment id (a 404) is indistinguishable from a bad id. Idempotency keys are
  scoped by `(agent_id, idempotency_key)`, so two agents may coincidentally
  pick the same string.
* Keys are never logged; `Debug` for the key set prints agent ids only.

**Decision ingestion**

* `crates/discord-gw` links `broker` as a library and calls
  `PaymentEngine::{approve, decline, report_outcome}` in-process. Both crates
  ship in the same container, so this is not a network boundary and adds no
  hop to the approval path.
* The engine never accepts a decision for a payment in a state that does not
  take it: it answers `DecisionResult::Expired`, which is the gateway's cue to
  reply ephemerally with "this request has expired" (spec §6) and to fill
  nothing.
* **Amended 2026-11-21 (bead till-7q6, ADR 0007):** `approve` carries the
  human's card choice — `approve(payment_id, Option<CardId>)`. The gateway
  encodes the card in the button's `custom_id` (`approve:<payment>:<card>`);
  with exactly one card registered it may send `None` and the engine
  auto-selects. A click that names no usable card answers
  `DecisionResult::Refused` ("pick a card"), which is deliberately distinct
  from `Expired`: the request has not expired, and the human is told the
  truth. The chosen card is committed in the same transition as the approval.
* Approve is the only path that can reach a card decryption: it constructs the
  `FillRequest` only after the `pending_approval → approved` transition has
  been committed with a `chosen_card`. Card values are therefore structurally
  unreachable before approval (AGENTS.md rule 7).

## Consequences

**Good**

* The approval trust boundary stays inside one process: no endpoint exists
  that an agent could call to approve its own payment, and no shared database
  write lets the gateway skip a legal transition.
* The plugin (till-7) and the gateway (till-6) now have a concrete, testable
  contract instead of an implied one.
* Fail-closed keys mean an operator error degrades to "broker refuses
  everything" rather than "broker trusts everyone".

**Bad / accepted costs**

* In-process calls mean the gateway cannot be deployed as an independent
  service without a new decision (an authenticated loopback endpoint or a
  decision queue). That is a real constraint, accepted because the baseline
  deployment is a single sidecar container.
* A single bearer token per agent is coarse: no scopes, no per-tool limits,
  no rotation story beyond editing the environment. Adequate for one trusted
  agent on a test host; revisit if the broker is ever multi-tenant.
* Constant-time comparison over a `Vec` of keys is O(n) per request. With a
  handful of agents this is irrelevant.
* An unset `TILL_AGENT_KEYS` produces a broker that answers 401 to
  everything — deliberately noisy rather than silently permissive, but it
  must be documented in the deployment runbook (docker/ bead).

## Alternatives considered

* **A single shared broker token** — cannot scope idempotency or payment
  visibility per agent, and makes a leaked token a full compromise. Rejected.
* **mTLS / signed requests** — stronger, but a certificate story for a
  single-host test sidecar, and the agent is not a mutually-authenticated
  principal yet (that is the tier-2 session-locking work in ADR 0005).
* **Gateway writes payment rows directly** — skips the state machine (the
  product), splits the transition logic across two crates, and puts card-fill
  authorization outside the component that owns it. Rejected outright.
* **Authenticated loopback HTTP endpoint for decisions** — allows independent
  deployment, but adds a second credential and a network hop on the approval
  path today, for a deployment split that does not exist yet. Deferred, not
  rejected; this ADR must be amended if it happens.
* **Polling the Discord API from the broker instead of a gateway** — inverts
  who owns the bot token and duplicates serenity wiring. Rejected.

## References

* `docs/SPEC.md` §2 (principals), §5 (`request_payment`), §6 (stale clicks),
  D4 (per-agent keys), US-13
* AGENTS.md rules 3, 6, 7 (submit-once, card-data egress, approval gating)
* ADR 0004 (the wait endpoint these keys also protect), ADR 0005 (fill
  channel), ADR 0001 (state durability)
