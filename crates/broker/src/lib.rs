//! till broker core.
//!
//! Payment state machine, HTTP API, persistence and crypto. See
//! `docs/SPEC.md` for the canonical design; see `AGENTS.md` for the
//! security rules that govern every line added to this crate.
//!
//! # Layout
//!
//! * [`state`] — the state machine itself (spec §4), a pure transition table.
//! * [`payment`] — the DTOs the agent may send and read back (no card data).
//! * [`store`] — SQLite persistence, idempotency index, submit-once latch
//!   (ADR 0001).
//! * [`ports`] — Camofox and Discord behind traits, so the machine is testable
//!   without either (spec §7).
//! * [`wait`] — the in-process long-poll registry (ADR 0004).
//! * [`engine`] — the transitions, the approval timer, the fill→submit
//!   pipeline and crash recovery.
//! * [`http`] — the three agent-facing endpoints, keyed per agent (ADR 0006).
//!
//! # Security boundary
//!
//! Card data never leaves the broker (AGENTS.md rules 6–7, spec §7). This crate
//! does not hold card values at all: it stores selectors, and the sole egress
//! of a decrypted value is the Camofox fill call for a payment that reached
//! `approved` through the human's single Approve click.

pub mod auth;
pub mod engine;
pub mod http;
pub mod payment;
pub mod ports;
pub mod state;
pub mod store;
pub mod wait;

pub use auth::{ApiKeys, AuthConfigError, AuthenticatedAgent};
pub use engine::{CreateOutcome, DecisionResult, EngineConfig, EngineError, PaymentEngine};
pub use http::{AppState, CreateResponse, router};
pub use payment::{
    Cart, CartItem, FieldKind, FieldSpec, InvalidOrderReference, NewPayment, OrderReference,
    Outcome, OutcomeKind, PaymentId, PaymentView, TerminalReason,
};
pub use ports::{
    ApprovalRequest, BoxFuture, CamofoxPort, DiscordPort, FillRequest, PortError,
    ResultReviewRequest,
};
pub use state::{IllegalTransition, PaymentEvent, PaymentState, transition};
pub use store::{SqliteStore, StoreError, StoredPayment, TransitionEffect};
pub use wait::WaitRegistry;
