# Payment Broker — Baseline Spec & User Stories

**Status:** Draft design snapshot — aligned between Matt Lorentz and nimbleco-hermes 2026-11-07.
Moved into this repository 2026-11-14; **this file is now the canonical home**
of the user stories and implementation decisions. The original
contractor-memory document is superseded here.

**Project name:** **till** — the cash register drawer. Money sits in the
drawer while the sale is still in progress; when the sale is confirmed,
the drawer closes.

**Thread origin:** Cyborg Garden Discord, #bounties-work, "Research →
purchase agentic flow" (1562280686734409789).

## 1. Purpose

Replace the `escalate_to_human(kind="confirmation")` path from swarm-map
PR #55 with a dedicated **payment broker sidecar** that:

- Stores card details encrypted at rest, outside the agent's readable
  namespace.
- Obtains approval **directly between the human and the broker** over
  Discord signed button interactions — the agent is not inside the
  approval trust boundary.
- Fills card fields and submits the checkout form programmatically via
  Camofox, using selectors the agent supplies. Card digits never enter
  model context.
- Classifies the post-submit outcome **with a second human review of a
  result screenshot** — the broker makes no automatic success/failure
  judgement.

The broker is **LLM-free**. A future hardening pass may add a small
isolated verifier LLM, but it is explicitly out of scope for the
baseline.

## 2. Architecture

```
Hermes agent (Jamie, test agent on the Mini)
   │  tools: request_payment / wait_for_payment / get_payment
   │  (thin authenticated HTTP client plugin — new, small)
   ▼
Payment broker (Docker container on the Mac Mini)
   │  • holds card data encrypted at rest
   │  • exposes HTTP API keyed per-agent
   │  • owns all payment state and policy
   ├──▶ Camofox (existing) — fills fields, submits, screenshots
   └──▶ Discord gateway (new bot) — approval + result-review interactions
```

PR #55's `human_escalation` plugin remains responsible for captcha,
SMS/2FA, and VNC link handoffs. Only the payment-confirmation path is
replaced.

## 3. Decisions Locked In

| # | Decision | Choice |
|---|----------|--------|
| D1 | Approval transport | Discord bot with signed button interactions |
| D2 | Broker intelligence | LLM-free; agent supplies selectors, broker executes |
| D3 | Human verification channel | Broker-captured screenshot of the checkout page, sent with the approval request |
| D4 | Broker placement | Docker sidecar on the Mac Mini; per-agent API keys |
| D5 | Encryption at rest | Yes — defends against env-scraping / log disclosure, **not** against an attacker with container access |
| D6 | Approval rounds | One. Single Approve authorizes fill + submit |
| D7 | Outcome retrieval | **Polling** (`wait_for_payment`) — long-poll with bounded timeout. Broker-initiated outcome messages to the agent's thread are **deferred**. |
| D8 | Hold-timer handling | Merchant hold **is** the approval deadline. Broker does not artificially keep the page warm. |
| D9 | Result classification | **Human reviews a post-submit screenshot** and clicks one of three buttons (see §6). No automatic success selectors. |
| D10 | First test target | **Local mock checkout site** with four programmable modes. Real merchants and Moshtix egress are separate workstreams. |
| D11 | Recovery semantics | Polling retrieves broker state. It does **not** restore a lost Camofox tab or agent context. Idempotency key dedupes `POST /payments` only — there is **no merchant-side exactly-once guarantee**. |

**Decisions added 2026-11-14 (repo kickoff):**

| # | Decision | Choice |
|---|----------|--------|
| D12 | Implementation language | Rust; axum, tokio, serenity, rusqlite (WAL), age encryption at rest |
| D13 | Discord library | serenity (no command framework — the bot only sends component messages and handles interactions) |
| D14 | Storage | rusqlite over redb/sled for inspectability (`sqlite3 file.db` in debug sessions) |
| D15 | Repo | `mplorentz/till`, AGPL-3.0; may move to cyborg-garden later |

## 4. State Machine

```
pending_approval ──approve──▶ approved ──▶ filling ──▶ submitting
                                                             │
                                                             ▼
                                                  awaiting_result_review
                                                             │
                              ┌──────────────────────────────┼──────────────────────────────┐
                              ▼                              ▼                              ▼
                         succeeded                    failed_no_charge                   ambiguous

pending_approval ──decline──▶ declined
pending_approval ──timeout──▶ approval_timeout
pre-submit Camofox/tab error ─▶ aborted_before_submit
post-submit-maybe disconnect ─▶ ambiguous
broker crash during `submitting` (recovered) ─▶ ambiguous
```

Non-terminal states: `pending_approval`, `approved`, `filling`,
`submitting`, `awaiting_result_review`.

Terminal states: `succeeded`, `failed_no_charge`, `ambiguous`, `declined`,
`approval_timeout`, `aborted_before_submit`.

`hold_expired` is folded into `approval_timeout` for the baseline — the
broker treats the merchant hold as the TTL and does not separately detect
mid-wait expiry.

## 5. Agent Tool Interface

A small new Hermes plugin (separate from `human_escalation`) exposing
three tools. The plugin is a thin authenticated HTTP client; the broker
owns all state.

### `request_payment(cart, tab_id, fields, submit_selector, idempotency_key) -> payment_id`

- `cart`: structured summary `{merchant_origin, items[], total, currency}`
  — shown to the human alongside the broker-captured screenshot.
- `tab_id`: the Camofox tab the agent has driven to the
  checkout-confirm screen.
- `fields`: list of `{selector, field_kind}` where
  `field_kind ∈ {card_number, expiry, cvv, cardholder_name,
  billing_postcode}`. The broker supplies the values; the agent never
  sees them.
- `submit_selector`: the final pay button.
- `idempotency_key`: agent-generated, dedupes the POST. Does not protect
  against merchant-side duplicate charges.

Returns `payment_id` synchronously. Broker immediately captures a
screenshot and posts the approval request to its configured Discord
destination.

### `wait_for_payment(payment_id, timeout_seconds) -> status`

Long-polls the broker. Returns when the state changes or
`timeout_seconds` elapses. Non-terminal returns let the agent re-invoke
without burning LLM tokens on a sleep loop. The agent's turn stays alive
across polls — we do **not** use PR #55's resume hook for the baseline.

### `get_payment(payment_id) -> status`

Immediate (non-blocking) read. Used for recovery if the agent restarts
and still has a `payment_id` from before.

## 6. Approval & Result-Review UX

### Approval message (broker → Discord)

- Structured cart summary (from the agent).
- Broker-captured screenshot of the checkout page (taken at
  approval-request time, not by the agent).
- Buttons: **Approve** / **Decline**.
- TTL display: when the merchant hold expires.

### Result-review message (broker → Discord, after submit)

- Broker-captured screenshot of the post-submit page. **Payment fields
  must be cleared or masked before capture** so card values never reach
  Discord or broker logs.
- Buttons:
  - **Order confirmed** → `succeeded` (broker records optional order
    reference if the human types one).
  - **Failed — no order/charge** → `failed_no_charge`.
  - **Outcome unclear** → `ambiguous`.

The result review is **not** a second payment approval — it is outcome
classification only. The single Approve in step 1 already authorized
fill + submit.

### Stale-interaction handling

Button interactions carry the `payment_id`. The broker rejects clicks on
already-terminal payments with an ephemeral "this request has expired"
message. Late Approve clicks after `approval_timeout` are rejected and
**nothing is filled or submitted**.

## 7. Camofox Interaction Rules

- **Card data never leaves the broker.** The sole permitted egress of
  decrypted card values is the Camofox fill call for a specific
  payment — never Discord, the Hermes plugin, API responses, or logs
  (ciphertext at rest is the only persisted form).
- **Card data reaches Camofox only after explicit owner approval.**
  Card values are sent only when the specific payment has reached
  `approved` via the human's single Approve click on that payment —
  never before approval, never speculatively (no pre-fill while
  waiting), and never on a late post-`approval_timeout` Approve.
- **Field filling uses `evaluate`, not `/type`.** The current Camofox
  `/type` endpoint emits entered text onto the plugin event bus — card
  data must not pass through it.
- The submit click is executed **exactly once** per payment. If the
  broker loses its connection to Camofox after clicking but before
  observing the outcome, the payment transitions to `ambiguous`.
- On success-path completion (after result review), the broker navigates
  the tab to `about:blank` before releasing it back to the agent. This
  makes "agent can't read the card back" a **structural** property for
  the success path rather than pure trust. The residual trust covers
  only the seconds between fill and submit.

## 8. Interim Trust Assumptions (carried forward from 2026-10-27)

These are explicit baseline assumptions, not hard security guarantees:

1. **Agent-reported cart accuracy.** The approval UI relies on the agent
   to report item and amount accurately. The broker can independently
   establish the tab's origin and captures a screenshot for human
   verification, but initially it does not independently prove the
   agent-reported total matches the checkout page. The screenshot is
   the human's check.
2. **Agent does not read card values from the DOM during the
   fill→submit window.** The skill instructs the agent not to; the broker
   navigates to `about:blank` afterward to bound the window. Future
   hardening: broker-exclusive tab lock in Camofox (requires
   per-principal auth Camofox does not currently have — see 2026-10-26
   analysis).

## 9. Mock Checkout Site

A small local website the broker team controls, used for the first
end-to-end test. Four programmable modes:

| Mode | Behavior | Expected broker outcome |
|------|----------|------------------------|
| `success` | Shows order reference on submit | `succeeded` (after human confirms screenshot) |
| `declined` | Shows "card declined" error | `failed_no_charge` |
| `ambiguous` | Indefinite "processing…" spinner | `ambiguous` (human picks "Outcome unclear") |
| `short_hold` | 60-second checkout TTL | `approval_timeout` if human is slow |

The mock site must expose a **submission counter** (e.g. a `/stats`
endpoint) so tests can assert the submit button was clicked exactly
once per payment, including across broker restarts.

## 10. User Stories

### Agent stories

- **US-1.** As Jamie, when I reach the checkout-confirm screen on a
  supported site, I call `request_payment` with the cart summary, field
  selectors, and submit selector, and receive a `payment_id`.
- **US-2.** As Jamie, after calling `request_payment`, I call
  `wait_for_payment` in a bounded loop until the broker returns a
  terminal state, then report that outcome to the user in plain language.
- **US-3.** As Jamie, when the broker returns `ambiguous`, I tell the
  user "the payment may have gone through — please check your statement
  / the merchant before we try again." I never auto-retry.
- **US-4.** As Jamie, when the broker returns `approval_timeout` or
  `aborted_before_submit`, I offer to restart the checkout from the
  event page.
- **US-5.** As Jamie, when the broker returns `declined`, I stop and do
  not retry without an explicit new instruction from the user.

### Broker stories

- **US-6.** As the broker, when I receive `POST /payments`, I capture a
  screenshot of the named Camofox tab and post an approval request to my
  configured Discord destination with the structured cart and
  Approve/Decline buttons.
- **US-7.** As the broker, when the human clicks Approve, I fill the card
  fields via Camofox `evaluate` and click submit exactly once.
- **US-8.** As the broker, when the human clicks Decline, I transition
  to `declined` and never touch the card fields.
- **US-9.** As the broker, when the approval TTL elapses with no
  decision, I transition to `approval_timeout` and reject any late
  Approve click.
- **US-10.** As the broker, after the submit click and page settle, I
  capture a result screenshot (payment fields cleared/masked) and post it
  with the three outcome buttons.
- **US-11.** As the broker, when the human picks an outcome, I transition
  to the corresponding terminal state and navigate the tab to
  `about:blank`.
- **US-12.** As the broker, if I crash and recover while a payment is in
  `submitting`, I transition it to `ambiguous` — I cannot prove whether
  the click landed.
- **US-13.** As the broker, I dedupe `POST /payments` by
  `idempotency_key` so an agent retry does not create a second payment.
- **US-14.** As the broker, I never write card digits to any log, event
  bus, or Discord message.

### Human stories

- **US-15.** As Juniper, when an agent wants to buy something, I see a
  Discord message from the broker bot with a screenshot of the actual
  checkout page and a structured cart summary, and I click Approve or
  Decline.
- **US-16.** As Juniper, after the broker submits, I see a second
  screenshot of the result page and click one of three buttons to record
  what happened.
- **US-17.** As Juniper, if I'm unsure whether the order went through, I
  click "Outcome unclear" and the agent tells me to verify before any
  retry.

## 11. Explicit Non-Goals (Baseline)

- Broker-initiated outcome messages into the agent's Discord thread (the
  "callback" pattern). Deferred — polling is sufficient.
- Automatic success/failure classification via per-site selectors.
- Broker-exclusive tab locking in Camofox (requires per-principal auth).
- A second isolated verifier LLM inside the broker.
- Moshtix residential-proxy egress. Ticketek anything.
- Real-merchant testing — mock site only until the state machine is
  proven.
- 3DS / bank-challenge handling (never reached in any prior attempt).
- Delegated KYA / agent wallets (roadmap horizon, separate thread).

## 12. Open Questions for Juniper

1. Which Discord channel is the broker's approval destination —
   `#approvals`, or a new dedicated channel?
2. Who besides Juniper is on the approver allowlist for the broker's
   Discord bot?
3. Test card for the post-mock phase: capped virtual card from Juniper,
   or a Stripe/similar test-mode card?
4. ~~Broker source repo~~ — **resolved 2026-11-14: standalone
   `mplorentz/till` (this repo), AGPL-3.0.**

## 13. Implementation Order (as filed in beads)

1. **till-2** — Mock checkout site with the four modes + submission
   counter.
2. **till-3/4** — Broker skeleton: HTTP API, payment state machine,
   persistence + crypto.
3. **till-5** — Camofox fill-via-evaluate + submit-once + screenshot
   capture.
4. **till-6** — Discord gateway: approval message + button handling.
5. **till-6** — Result-review message + button handling.
6. **till-7** — Jamie-side plugin with the three tools.
7. **till-8** — End-to-end test against the mock site covering all four
   modes and the crash-recovery path.

Each step is independently testable; steps 3–5 can be developed against
the mock site without Jamie in the loop. **till-1** (the Camofox
verification spike) runs first and gates the rest.
