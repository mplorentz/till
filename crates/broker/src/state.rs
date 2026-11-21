//! The payment state machine (spec §4) as a pure, total function.
//!
//! ```text
//! pending_approval ──approve──▶ approved ──▶ filling ──▶ submitting
//!                                                              │
//!                                                              ▼
//!                                                   awaiting_result_review
//!                                                              │
//!                               ┌──────────────────────────────┼──────────────────────────────┐
//!                               ▼                              ▼                              ▼
//!                          succeeded                    failed_no_charge                   ambiguous
//!
//! pending_approval ──decline──▶ declined
//! pending_approval ──timeout──▶ approval_timeout
//! pre-submit Camofox/tab error ─▶ aborted_before_submit
//! post-submit-maybe disconnect ─▶ ambiguous
//! broker crash during `submitting` (recovered) ─▶ ambiguous
//! ```
//!
//! Non-terminal states: `pending_approval`, `approved`, `filling`,
//! `submitting`, `awaiting_result_review`.
//!
//! Terminal states: `succeeded`, `failed_no_charge`, `ambiguous`, `declined`,
//! `approval_timeout`, `aborted_before_submit`.
//!
//! `hold_expired` is folded into `approval_timeout` for the baseline (spec §4).
//!
//! Nothing here knows about Camofox, Discord, HTTP or card data: it is the
//! transition table and nothing else, so the state machine stays the product
//! (AGENTS.md "Working rules").

use serde::{Deserialize, Serialize};

use crate::payment::OutcomeKind;

/// One payment state, exactly as named on the wire (spec §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaymentState {
    /// Waiting for the human's Approve/Decline click. Nothing has been filled.
    PendingApproval,
    /// The human clicked Approve; fill + submit are authorized.
    Approved,
    /// The broker is typing card values into the merchant page.
    Filling,
    /// The submit-once latch is committed and the click is in flight.
    Submitting,
    /// Submitted; waiting for the human to classify a result screenshot.
    AwaitingResultReview,
    /// Terminal: the human confirmed the order.
    Succeeded,
    /// Terminal: the human confirmed there is no order/charge.
    FailedNoCharge,
    /// Terminal: the outcome is unknown; never retried automatically.
    Ambiguous,
    /// Terminal: the human declined. The card fields were never touched.
    Declined,
    /// Terminal: the approval hold elapsed. A late Approve is rejected.
    ApprovalTimeout,
    /// Terminal: a pre-submit Camofox/tab error; nothing was submitted.
    AbortedBeforeSubmit,
}

impl PaymentState {
    /// Whether the payment can no longer change state.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded
                | Self::FailedNoCharge
                | Self::Ambiguous
                | Self::Declined
                | Self::ApprovalTimeout
                | Self::AbortedBeforeSubmit
        )
    }

    /// The wire name, which is also the value stored in SQLite.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PendingApproval => "pending_approval",
            Self::Approved => "approved",
            Self::Filling => "filling",
            Self::Submitting => "submitting",
            Self::AwaitingResultReview => "awaiting_result_review",
            Self::Succeeded => "succeeded",
            Self::FailedNoCharge => "failed_no_charge",
            Self::Ambiguous => "ambiguous",
            Self::Declined => "declined",
            Self::ApprovalTimeout => "approval_timeout",
            Self::AbortedBeforeSubmit => "aborted_before_submit",
        }
    }

    /// Parse a stored/wire state name.
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "pending_approval" => Self::PendingApproval,
            "approved" => Self::Approved,
            "filling" => Self::Filling,
            "submitting" => Self::Submitting,
            "awaiting_result_review" => Self::AwaitingResultReview,
            "succeeded" => Self::Succeeded,
            "failed_no_charge" => Self::FailedNoCharge,
            "ambiguous" => Self::Ambiguous,
            "declined" => Self::Declined,
            "approval_timeout" => Self::ApprovalTimeout,
            "aborted_before_submit" => Self::AbortedBeforeSubmit,
            _ => return None,
        })
    }
}

impl std::fmt::Display for PaymentState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Something that asks the machine to move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaymentEvent {
    /// The human's single Approve click (spec §6, D6).
    Approve,
    /// The human's Decline click (US-8).
    Decline,
    /// The merchant hold / approval TTL elapsed (US-9).
    HoldExpired,
    /// The broker is about to start filling (post-approval only).
    FillStarted,
    /// Every field was typed successfully.
    FieldsFilled,
    /// A pre-submit Camofox/tab error; nothing was submitted.
    FillFailed,
    /// The submit-once latch is committed; the click is in flight.
    SubmitAttempted,
    /// The click returned and the page has settled.
    SubmitCompleted,
    /// We cannot prove whether the click landed (spec §7).
    ClickUnconfirmed,
    /// The human classified the result screenshot (spec §6).
    Outcome(OutcomeKind),
    /// A restart found the payment in flight (US-12).
    CrashRecovered,
}

/// Why a transition was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("illegal transition: {from:?} on {event:?}")]
pub struct IllegalTransition {
    /// The state the payment was in.
    pub from: PaymentState,
    /// The event that was refused.
    pub event: PaymentEvent,
}

/// The whole state machine: one total function over (state, event).
///
/// # Errors
///
/// Returns [`IllegalTransition`] for every pair that spec §4 does not list.
/// In particular a late `Approve` (US-9) and any click on a terminal payment
/// (spec §6 stale interactions) are refused here, which is what guarantees
/// that nothing is filled after `approval_timeout`.
pub fn transition(
    from: PaymentState,
    event: PaymentEvent,
) -> Result<PaymentState, IllegalTransition> {
    use PaymentEvent as E;
    use PaymentState as S;

    let to = match (from, event) {
        (S::PendingApproval, E::Approve) => S::Approved,
        (S::PendingApproval, E::Decline) => S::Declined,
        (S::PendingApproval, E::HoldExpired) => S::ApprovalTimeout,

        (S::Approved, E::FillStarted) => S::Filling,
        // A fill error before any submit: nothing was typed or clicked.
        (S::Approved | S::Filling, E::FillFailed) => S::AbortedBeforeSubmit,
        (S::Filling, E::FieldsFilled) => S::Submitting,
        (S::Filling, E::SubmitAttempted) => S::Submitting,

        (S::Submitting, E::SubmitCompleted) => S::AwaitingResultReview,
        (S::Submitting, E::ClickUnconfirmed) => S::Ambiguous,

        (S::AwaitingResultReview, E::Outcome(OutcomeKind::Succeeded)) => S::Succeeded,
        (S::AwaitingResultReview, E::Outcome(OutcomeKind::FailedNoCharge)) => S::FailedNoCharge,
        (S::AwaitingResultReview, E::Outcome(OutcomeKind::Ambiguous)) => S::Ambiguous,

        // Crash recovery (US-12, ADR 0001): pre-submit is aborted, in-flight
        // submit is ambiguous because we cannot prove whether the click landed.
        (S::Approved | S::Filling, E::CrashRecovered) => S::AbortedBeforeSubmit,
        (S::Submitting, E::CrashRecovered) => S::Ambiguous,

        _ => return Err(IllegalTransition { from, event }),
    };

    debug_assert!(
        to.is_terminal() || !from.is_terminal(),
        "no transition may leave a terminal state"
    );
    Ok(to)
}

#[cfg(test)]
mod tests {
    use super::{PaymentEvent, PaymentState, transition};
    use crate::payment::OutcomeKind;

    const ALL_STATES: [PaymentState; 11] = [
        PaymentState::PendingApproval,
        PaymentState::Approved,
        PaymentState::Filling,
        PaymentState::Submitting,
        PaymentState::AwaitingResultReview,
        PaymentState::Succeeded,
        PaymentState::FailedNoCharge,
        PaymentState::Ambiguous,
        PaymentState::Declined,
        PaymentState::ApprovalTimeout,
        PaymentState::AbortedBeforeSubmit,
    ];

    const ALL_EVENTS: [PaymentEvent; 12] = [
        PaymentEvent::Approve,
        PaymentEvent::Decline,
        PaymentEvent::HoldExpired,
        PaymentEvent::FillStarted,
        PaymentEvent::FieldsFilled,
        PaymentEvent::FillFailed,
        PaymentEvent::SubmitAttempted,
        PaymentEvent::SubmitCompleted,
        PaymentEvent::ClickUnconfirmed,
        PaymentEvent::Outcome(OutcomeKind::Succeeded),
        PaymentEvent::Outcome(OutcomeKind::FailedNoCharge),
        PaymentEvent::Outcome(OutcomeKind::Ambiguous),
    ];

    /// The spec §4 table, written out as data.
    #[test]
    fn transition_table_matches_spec_4() {
        use PaymentEvent as E;
        use PaymentState as S;

        let legal = [
            ((S::PendingApproval, E::Approve), S::Approved),
            ((S::PendingApproval, E::Decline), S::Declined),
            ((S::PendingApproval, E::HoldExpired), S::ApprovalTimeout),
            ((S::Approved, E::FillStarted), S::Filling),
            ((S::Approved, E::FillFailed), S::AbortedBeforeSubmit),
            ((S::Filling, E::FieldsFilled), S::Submitting),
            ((S::Filling, E::SubmitAttempted), S::Submitting),
            ((S::Filling, E::FillFailed), S::AbortedBeforeSubmit),
            ((S::Submitting, E::SubmitCompleted), S::AwaitingResultReview),
            ((S::Submitting, E::ClickUnconfirmed), S::Ambiguous),
            ((S::Approved, E::CrashRecovered), S::AbortedBeforeSubmit),
            ((S::Filling, E::CrashRecovered), S::AbortedBeforeSubmit),
            ((S::Submitting, E::CrashRecovered), S::Ambiguous),
            (
                (S::AwaitingResultReview, E::Outcome(OutcomeKind::Succeeded)),
                S::Succeeded,
            ),
            (
                (
                    S::AwaitingResultReview,
                    E::Outcome(OutcomeKind::FailedNoCharge),
                ),
                S::FailedNoCharge,
            ),
            (
                (S::AwaitingResultReview, E::Outcome(OutcomeKind::Ambiguous)),
                S::Ambiguous,
            ),
        ];

        for from in ALL_STATES {
            for event in ALL_EVENTS {
                let expected = legal
                    .iter()
                    .find(|((f, e), _)| *f == from && *e == event)
                    .map(|(_, to)| *to);
                match expected {
                    Some(to) => assert_eq!(
                        transition(from, event),
                        Ok(to),
                        "{from:?} on {event:?} must reach {to:?}"
                    ),
                    None => assert!(
                        transition(from, event).is_err(),
                        "{from:?} on {event:?} must be refused"
                    ),
                }
            }
        }
    }

    /// Every outcome button maps to its own terminal state (spec §6).
    #[test]
    fn outcome_events_are_terminal_and_distinct() {
        let review = PaymentState::AwaitingResultReview;
        assert_eq!(
            transition(review, PaymentEvent::Outcome(OutcomeKind::Succeeded)),
            Ok(PaymentState::Succeeded)
        );
        assert_eq!(
            transition(review, PaymentEvent::Outcome(OutcomeKind::FailedNoCharge)),
            Ok(PaymentState::FailedNoCharge)
        );
        assert_eq!(
            transition(review, PaymentEvent::Outcome(OutcomeKind::Ambiguous)),
            Ok(PaymentState::Ambiguous)
        );
    }

    /// A late Approve after the hold expired can never authorize a fill (US-9).
    #[test]
    fn approve_is_refused_once_the_hold_expired() {
        assert!(
            transition(PaymentState::ApprovalTimeout, PaymentEvent::Approve).is_err(),
            "approval_timeout must refuse Approve"
        );
        assert!(
            transition(PaymentState::Declined, PaymentEvent::Approve).is_err(),
            "declined must refuse Approve"
        );
        assert!(
            transition(PaymentState::AbortedBeforeSubmit, PaymentEvent::Approve).is_err(),
            "aborted_before_submit must refuse Approve"
        );
    }

    /// Terminal states never move, whatever arrives (spec §6 stale clicks).
    #[test]
    fn terminal_states_never_transition() {
        for from in ALL_STATES.into_iter().filter(|state| state.is_terminal()) {
            for event in ALL_EVENTS {
                assert!(
                    transition(from, event).is_err(),
                    "{from:?} must refuse {event:?}"
                );
            }
            assert!(
                transition(from, PaymentEvent::CrashRecovered).is_err(),
                "{from:?} must survive recovery unchanged"
            );
        }
    }

    /// The wire names round-trip through the database representation.
    #[test]
    fn state_names_round_trip() {
        for state in ALL_STATES {
            assert_eq!(PaymentState::parse(state.as_str()), Some(state));
        }
        assert_eq!(PaymentState::parse("hold_expired"), None);
    }
}
