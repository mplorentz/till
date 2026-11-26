# ADR 0002 — Encrypt card blobs at rest with age (ciphertext only on disk)

- **Status:** Accepted
- **Date:** 2026-11-14
- **Bead:** till-a0q (spike) → applies to till-1gt
- **Spec:** §3 D5, §7; AGENTS.md security rules 1, 6

## Context

The broker must hold real card details — it is the only component
allowed to see them — but the threat being defended against is narrow
and explicitly stated in D5: **environment scraping and log disclosure,
not an attacker with container access.** The card store is therefore
encrypted-at-rest rather than key-managed by an HSM.

Hard requirements from the spec and repo rules:

* Card digits must never exist in plaintext in a log, trace, API
  response, error message, Discord message, or the Hermes plugin.
* **Ciphertext is the only persisted form** (AGENTS.md rule 6).
* Decryption is permitted for exactly one purpose: the Camofox fill call
  for a payment in `approved` state (AGENTS.md rule 7).
* No new dependency without justification, especially anything touching
  card data.

The spike sharpened the log/telemetry requirement with two concrete
leaks that the broker cannot fix by choosing a different endpoint:

* Camofox's `POST /tabs/:id/type` publishes the typed `text` on the
  plugin event bus.
* Camofox's `POST /tabs/:id/evaluate` publishes **both** the expression
  (which contains the inlined card values) **and** the result; and when
  Playwright tracing is enabled on the session, the fill `value` /
  `evaluateExpression` parameters are written as plaintext to
  `trace-*.zip` on disk.

So "secure at rest" must cover *our* storage, and the design must also
refuse to hand card data to a component that writes plaintext traces.

## Decision

Encrypt card details with **`age`** (X25519 recipients, ASCII-armored
blob) and persist **only the ciphertext**.

* **Key lifecycle (amended 2026-11-21, overseer): no manual keygen and no
external key store.** The broker generates its own `age` identity the first
time it needs one, writes it to its data volume (`0600`), and reuses it
forever after (`CardCipher::load_or_generate`). Generate **once, when
absent** — regenerating on every boot would make every stored card
undecryptable. Losing the volume means re-entering the cards; the human keeps
the real cards elsewhere, so that is an acceptable failure, not a payment
safety event. The D5 tradeoff is unchanged and still the whole claim: this
defends against environment scraping and log disclosure, **not** against an
attacker with access to the volume or the container.
* **One card per row (amended 2026-11-21).** The vault is a small set of
long-term cards (`cards` table, ADR 0007), not one blob bound to a payment.
The plaintext card object (`card_number`, `expiry`, `cvv`,
`cardholder_name`, `billing_postcode`) is serialized, encrypted to the
recipient, and stored as an opaque blob. **No column holds a PAN, expiry or
CVV in plaintext.**
* **Display data carve-out (amended 2026-11-21).** Two card fields *are*
stored in clear, because the approval flow needs them to name the card: the
operator's pet name and the **last four digits**. Both are PCI-permissible
display data — what a receipt or a card sleeve shows. No last-four is derived
from a stored plaintext PAN, and no other field is stored in clear.
* Plaintext material is wrapped in `secrecy::SecretString` /
`zeroize`-on-drop types (`crypto::CardRecord`), decrypted **only** inside the
fill path for a payment in `approved`, and dropped as soon as the Camofox fill
call returns.
* `Debug`/`Serialize`/`Display` are not implemented for the card types;
  logging is written so a card value cannot be interpolated into an
  error (`AGENTS.md` rule 1). Card values are never placed in an
  exception/error message, because the spike showed Camofox logs page
  exceptions verbatim.
* **Deployment rule that the crypto depends on:** the payment session in
  Camofox must not be traced (`trace` is only settable at tab creation
  and cannot be added later — 409), because tracing writes the fill
  value in plaintext to disk. Tracing is treated as card-data egress
  (see `docs/findings/camofox-spike.md`, finding A).

## Consequences

**Good**

* A disk snapshot, a stolen backup, or a `docker logs` dump does not
  yield card digits. That is precisely D5's threat model.
* `age` is small, audited, streaming-friendly, and has one primitive
  (X25519 + ChaCha20-Poly1305) instead of a cipher-suite knob to get
  wrong.
* Key rotation is a recipient-list change + re-wrap; no DB crypto
  re-architecture.
* The ciphertext-only rule makes "did we leak?" auditable: `grep -rE
  '[0-9]{13,19}'` over the data dir and logs should return nothing but
  base64 of armor headers.

**Bad / accepted costs**

* **Not a defence against container compromise**, and it is not sold as
  one: the decryption key lives in the same container and now in the same
  volume as the database. D5 says so explicitly; anyone claiming otherwise
  should read D5 first.
* The generated identity is a single point of failure: lose the volume, lose
the cards. (Accepted 2026-11-21 — cards are re-enterable by the owner; losing
them is a support task, not a payment-safety event.)
* The pet name and last four are readable without the key. That is the price
of naming the card in an approval message; it is display data, not a card
value.
* Encrypting on every write adds a decrypt/encrypt round trip in the
  fill path (~ms); irrelevant beside the ~2 s submit click measured in
  the spike.
* `age` is a new dependency — justified here because hand-rolling
  authenticated encryption is worse, and because the alternative
  (whole-database encryption) fights the inspectability decision in ADR
  0001.
* One ciphertext per card means rotating an individual card is a row rewrite,
  not an in-place field update.

## Alternatives considered

* **Plaintext in SQLite** — fails AGENTS.md rule 6 outright.
* **SQLCipher (encrypt the whole DB)** — protects everything, including
  state that does not need protecting, makes the `sqlite3` debug story
  from ADR 0001 impossible without the key, and adds a C dependency
  with a build-time crypto option. Also makes "ciphertext is the only
  persisted form" harder to *audit* (it is a property of a binary, not
  of a column).
* **OS keychain / KMS** — better separation of key and data, but adds a
  network/base-system dependency for a single-host sidecar whose threat
  model explicitly excludes host compromise. Revisit if the broker ever
  handles multiple owners' cards (the future KYA work).
* **Tokenization / never storing the PAN** — not available: the broker
  must fill arbitrary merchants' forms, which is the entire reason it
  holds the card. This is a roadmap item, not a baseline option.

## References

* `docs/SPEC.md` §3 D5, §7; AGENTS.md security rules 1, 5, 6, 7
* `docs/findings/camofox-spike.md` — Q2 (expression/result on the event
  bus, exception text in logs), finding A (plaintext fill values in
  Playwright traces)
* ADR 0001 (where the ciphertext lives)