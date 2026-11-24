# ADR 0007 — Long-term card vault, and the card choice happens at approval

- **Status:** Accepted
- **Date:** 2026-11-21
- **Bead:** till-7q6 (persistence + crypto)
- **Spec:** §5 (`request_payment`), §6 (approval UX); AGENTS.md rules 6–7;
  amends ADR 0001 (schema), ADR 0002 (key lifecycle), ADR 0006 (decision
  ingest)
- **Supersedes:** the earlier "cards are attached per payment" shape of the
  till-7q6 plan

## Context

The broker's purpose is a payment approved *from Discord*. An earlier shape of
this work attached a single card to each payment, which meant re-entering a
card for every purchase — the human had the card in hand anyway, so the broker
saved nothing. The overseer's requirement of 2026-11-21 changes the model:

1. The broker keeps a **small set of cards long-term**, each encrypted at rest.
2. Registration is **one `docker exec` per card**, interactive, with no card
   value in argv, no echo of the sensitive fields, and nothing logged.
3. The **human** chooses the card at approval time. The agent stays card-blind
   (`request_payment` gains no card parameter).

What made this a decision rather than an implementation detail: the card choice
moves into the approval interaction, which is the trust boundary for card
exposure (AGENTS.md rule 7). Whatever carries the choice must make "approved"
and "paying with this card" atomic, and it must not widen what the agent can
infer.

## Decision

**Vault.** One `cards` table: `card_id`, `pet_name`, `last4`, `blob`,
timestamps.

* `pet_name` and `last4` are **clear**: both are PCI-permissible display data,
  and both are needed to render the approval message. Nothing else about the
  card is stored in clear — not the expiry, not a second last-four.
* `blob` is the full card (`card_number`, `expiry`, `cvv`, `cardholder_name`,
  `billing_postcode`) serialized and encrypted with `age` to the deployment
  recipient. `pet_name` carries a `UNIQUE` index so the approval listing cannot
  show two identical entries.
* Plaintext lives only in `crypto::CardRecord` (`SecretString` fields,
  zeroize-on-drop, no `Debug`/`Display`/`Clone`/`Serialize`). It is constructed
  by the registration CLI, or by `CardCipher::decrypt` inside the approved fill
  path, and dropped as soon as the Camofox fill call returns.

**Registration.** `docker exec -it till till-card add`:
prompted fields, `rpassword` for the number/expiry/CVV (no echo), plain
prompts for the pet name, cardholder name and postcode. The CLI accepts **no**
card-shaped argument; unknown flags are a usage error, so `--pan` cannot ever
work. `till-card list` prints `card_id`, pet name and last four — the whole
listing surface. `till-card remove <card-id>` is the correction path.

**Approval.** The card is chosen by the human, in Discord:

* `ApprovalRequest` carries the listing (`CardSummary`: id, pet name, last4).
* **One card registered** → the message shows that card's pet name and last
  four with Approve/Decline, and `PaymentEngine::approve(payment, None)`
  auto-selects it.
* **Several cards** → one Approve button per card, labelled
  `Approve · Groceries ∙1234`, `custom_id = approve:<payment-uuid>:<card-uuid>`.
* `chosen_card` is written in the **same** transaction as
  `pending_approval → approved` (`TransitionEffect::chosen_card`), so there is
  no instant at which a payment is approved without the card it pays with.
* A click that names no usable card answers `DecisionResult::Refused`
  ("pick a card" / "no card registered"), never `Expired`, and nothing is
  filled or submitted.

**Buttons, not a select menu.** A string-select would need the selection to be
persisted between two interactions (the select arrives as its own interaction)
or the message to be edited: two round trips on the highest-stakes path, plus a
stale-selection race when the vault changes between the two clicks. A
per-card Approve button carries the choice in the click's `custom_id`, so
choice and approval are one atomic interaction, and `custom_id` limits are not
close (81 of 100 characters for two UUIDs). Discord allows five components per
action row; the baseline's "small set" fits in one row of Approve buttons plus
a second row for Decline. A card set too large to fit (roughly >10) is out of
baseline scope and would need the select-menu shape, which this ADR then has to
be amended to allow.

**Fill path.** The engine resolves `chosen_card` → `card_blob` → `decrypt` →
`FillRequest { card }` → `fill_fields`, then drops the record (zeroize) before
committing the submit latch. Any failure is `FillFailed` →
`aborted_before_submit`: nothing was clicked, so nothing can be charged.

## Consequences

**Good**

* Ordinary use is one Approve click; the card is chosen in the same gesture
  that authorises it.
* The agent's surface is unchanged: no card parameter, no card in
  `PaymentView`. A compromised agent can neither choose a card nor learn which
  one was used.
* "Approved with card X" is a single committed fact, readable with `sqlite3`
  (`chosen_card`) and replayed by crash recovery.
* The vault holds ciphertext; the only clear card fields are the two an
  operator would print on a card sleeve.

**Bad / accepted costs**

* `last4` and `pet_name` are stored in clear. That is a deliberate widening of
  ADR 0002's "ciphertext only", justified by PCI's display rule and by the
  approval flow's need to name the card. It reveals nothing a merchant receipt
  would not.
* A per-card Approve button multiplies buttons with the card set. Ten cards is
  the practical ceiling before a different component shape is needed.
* Registration is a host-side command: no remote card entry exists, by design.
* A card deleted from the vault cannot be the `chosen_card` of any payment (the
  foreign key refuses it), so removing a card never leaves a payment pointing
  at nothing.

## Alternatives considered

* **Select menu, then Approve** — rejected above: two interactions, persisted
  intermediate state, stale-selection race.
* **Card attached per payment** (the previous plan) — defeats the point of a
  quick Discord approval; re-entering card data per purchase is exactly the
  friction the requirement removes.
* **An HTTP endpoint for registration** — a card-value ingress on the network,
  reachable by the agent's key space if misconfigured. The CLI writes the
  database directly; there is no endpoint that accepts a card value.
* **Auto-selecting "the default card" with several registered** — a wrong-card
  charge is unrecoverable in the way an ambiguous submit is not. Refusing is
  the safe failure.

## References

* `docs/SPEC.md` §5, §6; AGENTS.md rules 1, 5, 6, 7
* ADR 0001 (schema, amended: `cards` + `chosen_card` land here), ADR 0002
  (crypto, amended: key lifecycle, per-card blobs, display data), ADR 0006
  (decision ingest, amended: `approve` carries the choice)
* `crates/broker/src/crypto.rs`, `crates/broker/src/bin/till-card.rs`,
  `crates/broker/src/store.rs` (`cards`, `chosen_card`)
