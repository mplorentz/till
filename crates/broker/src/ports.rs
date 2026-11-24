//! The two outside worlds, behind traits (spec §2, §7).
//!
//! The state machine must be testable without Camofox and without Discord, so
//! both live behind a trait object here and the only implementations in this
//! bead are the test driver in `tests/state_machine.rs`. The production
//! implementations arrive with their own beads (`crates/camofox-client`,
//! `crates/discord-gw`).
//!
//! # Card data boundary (AGENTS.md rules 6–7, ADR 0002/0005/0007)
//!
//! [`FillRequest`] is the single egress of decrypted card values. The engine's
//! fill path resolves the card the human chose at the approval click, decrypts
//! it, builds one `FillRequest`, hands it to the Camofox implementation, and
//! drops the record immediately afterwards (zeroize-on-drop). Two properties
//! fall out of that shape:
//!
//! * No card value exists in the engine outside that one call, in a
//!   `PaymentView`, in an error, or in a log line — [`crate::crypto::CardRecord`]
//!   implements no `Debug`, `Display` or `Serialize`, and `FillRequest`'s own
//!   `Debug` prints a redaction instead of the card.
//! * "Card data reaches Camofox only for an approved payment" is structural:
//!   the only place a `FillRequest` is built is [`crate::engine`]'s fill path,
//!   which runs after `pending_approval → approved` was committed with the
//!   chosen card.
//!
//! [`ApprovalRequest`] carries the card *listing* — pet name and last four
//! only — so the human can pick the card in Discord. That is display data,
//! not a card value (ADR 0007).

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use crate::crypto::CardRecord;
use crate::payment::{CardSummary, Cart, FieldSpec, PaymentId};

/// A boxed future, so the port traits stay object-safe (`Arc<dyn CamofoxPort>`)
/// without pulling in `async-trait`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A port failed. Carries no payload at all, so a port can never leak card data
/// into a broker log line through an error message (AGENTS.md rule 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("payment port unavailable")]
pub struct PortError;

/// What the broker asks Camofox to type: the selectors, and the values.
///
/// The `card` is the decrypted record for the card the human chose at
/// approval. It lives no longer than this request: the engine drops it as soon
/// as [`CamofoxPort::fill_fields`] returns.
#[derive(Clone, Copy)]
pub struct FillRequest<'a> {
    /// The payment this fill is for (`approved` by construction).
    pub payment_id: PaymentId,
    /// The tab to fill.
    pub tab_id: &'a str,
    /// Where each value goes.
    pub fields: &'a [FieldSpec],
    /// The values themselves (the chosen card).
    pub card: &'a CardRecord,
}

impl fmt::Debug for FillRequest<'_> {
    /// Prints the selectors and a redaction — never a card value, which is why
    /// this is hand-written rather than derived (AGENTS.md rule 1).
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FillRequest")
            .field("payment_id", &self.payment_id)
            .field("tab_id", &self.tab_id)
            .field("fields", &self.fields)
            .field("card", &"<redacted>")
            .finish()
    }
}

/// The Camofox side: fill and submit one payment (spec §7).
///
/// Implementors must fill via `POST /tabs/:id/type` and must never use
/// `evaluate` to carry card digits (ADR 0005).
pub trait CamofoxPort: Send + Sync + fmt::Debug {
    /// Type every field of the request into `tab_id`.
    fn fill_fields<'a>(&'a self, request: FillRequest<'a>) -> BoxFuture<'a, Result<(), PortError>>;

    /// Click the submit button exactly once (spec §7).
    fn click_submit<'a>(
        &'a self,
        payment_id: PaymentId,
        tab_id: &'a str,
        submit_selector: &'a str,
    ) -> BoxFuture<'a, Result<(), PortError>>;
}

/// The approval message the gateway must post (spec §6, US-6, ADR 0007). Cart,
/// deadline and the card listing — the screenshot is the gateway's business.
#[derive(Debug, Clone, Copy)]
pub struct ApprovalRequest<'a> {
    /// Which payment the buttons belong to.
    pub payment_id: PaymentId,
    /// The structured cart summary the human checks against the screenshot.
    pub cart: &'a Cart,
    /// When the hold expires, for the "TTL display" line (ADR 0004).
    pub approval_deadline: i64,
    /// The cards the human may pay with: pet name and last four only.
    pub cards: &'a [CardSummary],
}

/// The result-review message the gateway must post after submit (spec §6,
/// US-10). Payment id + cart only: any screenshot must be captured and masked
/// by the gateway/fill bead, never by the engine.
#[derive(Debug, Clone, Copy)]
pub struct ResultReviewRequest<'a> {
    /// Which payment the outcome buttons belong to.
    pub payment_id: PaymentId,
    /// The cart, repeated so the human knows what was bought.
    pub cart: &'a Cart,
}

/// The Discord side: post an approval request, then a result-review request.
pub trait DiscordPort: Send + Sync + fmt::Debug {
    /// Post the approval message with Approve/Decline buttons (US-6).
    fn request_approval(
        &self,
        request: ApprovalRequest<'_>,
    ) -> BoxFuture<'_, Result<(), PortError>>;

    /// Post the result-review message with the three outcome buttons (US-10).
    fn request_result_review(
        &self,
        request: ResultReviewRequest<'_>,
    ) -> BoxFuture<'_, Result<(), PortError>>;
}
