//! The two outside worlds, behind traits (spec §2, §7).
//!
//! The state machine must be testable without Camofox and without Discord, so
//! both live behind a trait object here and the only implementations in this
//! bead are the test driver in `tests/state_machine.rs`. The production
//! implementations arrive with their own beads (`crates/camofox-client`,
//! `crates/discord-gw`).
//!
//! # Card data boundary (AGENTS.md rules 6–7, ADR 0005)
//!
//! [`FillRequest`] deliberately carries **selectors only** — no values. The
//! engine builds one exactly once, immediately after the payment reached
//! `approved` through the human's single Approve click, and the Camofox
//! implementation resolves the actual card values from the encrypted store at
//! that moment. Two properties fall out of this shape:
//!
//! * No card value ever exists in the engine, in a `PaymentView`, in an error,
//!   or in a log line, because the engine has nowhere to put one.
//! * "Card data reaches Camofox only for an approved payment" is structural:
//!   the only constructor of [`FillRequest`] is [`crate::engine`]'s fill path,
//!   which runs after the `pending_approval → approved` transition.
//!
//! Today the type is constructible anywhere within the crate (a `pub` struct
//! with `pub` fields); when the fill implementation lands it should take an
//! opaque `ApprovedFill` ticket minted by the state machine, which closes that
//! gap completely.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use crate::payment::{Cart, FieldSpec, PaymentId};

/// A boxed future, so the port traits stay object-safe (`Arc<dyn CamofoxPort>`)
/// without pulling in `async-trait`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A port failed. Carries no payload at all, so a port can never leak card data
/// into a broker log line through an error message (AGENTS.md rule 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("payment port unavailable")]
pub struct PortError;

/// What the broker asks Camofox to type. Selectors only — never values.
#[derive(Debug, Clone, Copy)]
pub struct FillRequest<'a> {
    /// The payment this fill is for (`approved` by construction).
    pub payment_id: PaymentId,
    /// The tab to fill.
    pub tab_id: &'a str,
    /// Where each value goes.
    pub fields: &'a [FieldSpec],
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

/// The approval message the gateway must post (spec §6, US-6). Cart and
/// selectors only — the screenshot is the gateway's business.
#[derive(Debug, Clone, Copy)]
pub struct ApprovalRequest<'a> {
    /// Which payment the buttons belong to.
    pub payment_id: PaymentId,
    /// The structured cart summary the human checks against the screenshot.
    pub cart: &'a Cart,
    /// When the hold expires, for the "TTL display" line (ADR 0004).
    pub approval_deadline: i64,
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
