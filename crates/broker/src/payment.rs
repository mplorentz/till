//! Payment DTOs: what the agent may send, what the agent may read back.
//!
//! # Security boundary
//!
//! Nothing in this module holds a card value. The agent reports *selectors*,
//! the broker resolves *values* at fill time from the card vault
//! ([`crate::crypto`], AGENTS.md rules 6–7, spec §7, ADR 0002). [`PaymentView`]
//! is the only response body the HTTP API produces, and it is built from
//! cart/selector/state metadata only — there is no field on it that could
//! carry a PAN, expiry or CVV even by mistake.
//!
//! The one card-shaped thing here is [`CardSummary`]: a card id, the operator's
//! pet name, and the last four digits. That is PCI-permissible display data
//! (overseer requirement 2026-11-21) shown in the approval message so the human
//! can confirm which card pays; it is not a card value, and it never reaches
//! the agent-facing API.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A broker-minted payment identifier (unguessable, safe to echo to an agent).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PaymentId(Uuid);

impl PaymentId {
    /// Mint a fresh identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for PaymentId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for PaymentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

impl FromStr for PaymentId {
    type Err = InvalidPaymentId;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(value)
            .map(Self)
            .map_err(|_| InvalidPaymentId)
    }
}

/// The path segment was not a payment id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("malformed payment id")]
pub struct InvalidPaymentId;

/// A registered card's identifier (unguessable, safe to display to the human).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CardId(Uuid);

impl CardId {
    /// Mint a fresh identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for CardId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for CardId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

impl FromStr for CardId {
    type Err = InvalidCardId;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(value).map(Self).map_err(|_| InvalidCardId)
    }
}

/// The path segment was not a card id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("malformed card id")]
pub struct InvalidCardId;

/// What the approval flow may know about a card: pet name and last four.
///
/// This is the whole card-listing surface (overseer requirement 2026-11-21).
/// It carries no PAN, expiry or CVV — only the display data that PCI allows
/// and the human needs in order to pick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardSummary {
    /// The card id, used by the approving click.
    pub card_id: CardId,
    /// The operator's pet name for this card.
    pub pet_name: String,
    /// The last four digits, for confirmation.
    pub last4: String,
}

/// One machine-checkout field the broker is allowed to type into.
///
/// The variant names are the spec §5 `field_kind` values; the broker supplies
/// the value, the agent only ever sees the selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldKind {
    /// The primary account number.
    CardNumber,
    /// `MM/YY` (or the merchant's format).
    Expiry,
    /// The security code.
    Cvv,
    /// The name on the card.
    CardholderName,
    /// Billing postcode / ZIP.
    BillingPostcode,
}

/// A selector plus the kind of value that belongs in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldSpec {
    /// CSS selector of the input on the merchant page.
    pub selector: String,
    /// Which card value the broker must type there.
    pub field_kind: FieldKind,
}

/// One line item of the agent-reported cart (shown to the human, spec §6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CartItem {
    /// Human-readable item name.
    pub name: String,
    /// How many.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quantity: Option<u32>,
    /// Unit price as the merchant displays it (a string — never a float
    /// round-trip on money).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit_price: Option<String>,
}

/// The agent-reported cart summary `{merchant_origin, items[], total, currency}`
/// (spec §5). Reported by the agent, verified by the human against the
/// broker's screenshot (spec §8 assumption 1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cart {
    /// The checkout origin, e.g. `https://shop.example`.
    pub merchant_origin: String,
    /// The line items.
    pub items: Vec<CartItem>,
    /// The claimed total, as displayed.
    pub total: String,
    /// ISO-4217-ish currency code.
    pub currency: String,
}

/// The body of `POST /payments` (spec §5 `request_payment`).
///
/// Deliberately contains no card data: `fields` names *where* values go, not
/// *what* they are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewPayment {
    /// Agent-generated key that dedupes this POST (US-13).
    pub idempotency_key: String,
    /// The Camofox tab the agent has driven to the checkout-confirm screen
    /// (spec §5; required by ADR 0001's schema even though the converged
    /// design text elides it).
    pub tab_id: String,
    /// The cart summary the human will approve.
    pub cart: Cart,
    /// Where each card value goes.
    pub fields: Vec<FieldSpec>,
    /// The final pay button.
    pub submit_selector: String,
}

/// The human's result-review choice (spec §6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    /// "Order confirmed" — the human may type an order reference.
    Succeeded {
        /// Optional, sanitized order reference.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        order_reference: Option<OrderReference>,
    },
    /// "Failed — no order/charge".
    FailedNoCharge,
    /// "Outcome unclear". Never retried automatically (US-3).
    Ambiguous,
}

impl Outcome {
    /// The copyable discriminant the state machine transitions on.
    #[must_use]
    pub fn kind(&self) -> OutcomeKind {
        match self {
            Self::Succeeded { .. } => OutcomeKind::Succeeded,
            Self::FailedNoCharge => OutcomeKind::FailedNoCharge,
            Self::Ambiguous => OutcomeKind::Ambiguous,
        }
    }

    /// The order reference, when the human supplied one.
    #[must_use]
    pub fn order_reference(&self) -> Option<&OrderReference> {
        match self {
            Self::Succeeded { order_reference } => order_reference.as_ref(),
            Self::FailedNoCharge | Self::Ambiguous => None,
        }
    }
}

/// The three outcome buttons, without their payload (spec §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutcomeKind {
    /// Order confirmed.
    Succeeded,
    /// Failed with no charge.
    FailedNoCharge,
    /// Outcome unclear.
    Ambiguous,
}

/// A human-typed order reference, restricted to characters that cannot be a
/// card number fragment and short enough to be safe to store and log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct OrderReference(String);

impl OrderReference {
    /// Longest accepted reference.
    pub const MAX_LEN: usize = 64;

    /// Validate and wrap a human-typed reference.
    ///
    /// # Errors
    ///
    /// Rejects empty, over-long, or non-`[A-Za-z0-9._-]` input.
    pub fn parse(value: &str) -> Result<Self, InvalidOrderReference> {
        let trimmed = value.trim();
        if trimmed.is_empty()
            || trimmed.len() > Self::MAX_LEN
            || !trimmed
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            return Err(InvalidOrderReference);
        }
        Ok(Self(trimmed.to_string()))
    }

    /// The reference as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OrderReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl TryFrom<String> for OrderReference {
    type Error = InvalidOrderReference;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<OrderReference> for String {
    fn from(value: OrderReference) -> Self {
        value.0
    }
}

/// The reference failed validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid order reference")]
pub struct InvalidOrderReference;

/// Why a terminal state was reached, kept small and enumerable so no free text
/// (and therefore no card-shaped string) is ever persisted or returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalReason {
    /// The approval hold elapsed (US-9).
    HoldExpired,
    /// The human declined (US-8).
    DeclinedByHuman,
    /// A pre-submit Camofox/tab error.
    PreSubmitError,
    /// The submit click could not be confirmed (spec §7).
    UnconfirmedSubmit,
    /// A restart found the payment in flight (US-12).
    CrashRecovery,
    /// The human classified the result screenshot (spec §6).
    ResultReviewed,
}

impl TerminalReason {
    /// The stored/wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HoldExpired => "hold_expired",
            Self::DeclinedByHuman => "declined_by_human",
            Self::PreSubmitError => "pre_submit_error",
            Self::UnconfirmedSubmit => "unconfirmed_submit",
            Self::CrashRecovery => "crash_recovery",
            Self::ResultReviewed => "result_reviewed",
        }
    }

    /// Parse a stored name.
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "hold_expired" => Self::HoldExpired,
            "declined_by_human" => Self::DeclinedByHuman,
            "pre_submit_error" => Self::PreSubmitError,
            "unconfirmed_submit" => Self::UnconfirmedSubmit,
            "crash_recovery" => Self::CrashRecovery,
            "result_reviewed" => Self::ResultReviewed,
            _ => return None,
        })
    }
}

/// Everything the API will ever say about a payment: cart, selectors, state.
///
/// This is the security boundary in type form (overseer note 2026-11-14,
/// AGENTS.md rule 6): no card values, no last-four, no ciphertext.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentView {
    /// The payment id.
    pub payment_id: PaymentId,
    /// Current state.
    pub state: crate::state::PaymentState,
    /// The agent-reported cart.
    pub cart: Cart,
    /// Where each card value would go (selectors only).
    pub fields: Vec<FieldSpec>,
    /// The submit button selector.
    pub submit_selector: String,
    /// The Camofox tab this payment is bound to.
    pub tab_id: String,
    /// Unix seconds at which a pending approval expires.
    pub approval_deadline: i64,
    /// The human-typed order reference, after a `succeeded` review.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order_reference: Option<OrderReference>,
    /// Why the payment is terminal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<TerminalReason>,
    /// Unix seconds the payment was created.
    pub created_at: i64,
    /// Unix seconds of the last transition.
    pub updated_at: i64,
}
