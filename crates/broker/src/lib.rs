//! till broker core.
//!
//! Payment state machine, HTTP API, persistence and crypto. See
//! `docs/SPEC.md` for the canonical design; see `AGENTS.md` for the
//! security rules that govern every line added to this crate.
//!
//! # Layout
//!
//! * [`state`] — the state machine itself (spec §4), a pure transition table.
//! * [`artifacts`] — the masked-screenshot store the gateway reads by payment
//!   id (ADR 0008).
//! * [`payment`] — the DTOs the agent may send and read back (no card data).
//! * [`store`] — SQLite persistence, idempotency index, submit-once latch,
//!   the long-term card vault (ADR 0001/0002).
//! * [`crypto`] — the age cipher: cards in, signed ciphertext out, and the key
//!   lifecycle (generate once, when absent — ADR 0002).
//! * [`ports`] — Camofox and Discord behind traits, so the machine is testable
//!   without either (spec §7).
//! * [`wait`] — the in-process long-poll registry (ADR 0004).
//! * [`engine`] — the transitions, the approval timer, the fill→submit
//!   pipeline and crash recovery.
//! * [`http`] — the three agent-facing endpoints, keyed per agent (ADR 0006).
//!
//! # Security boundary
//!
//! Card data never leaves the broker (AGENTS.md rules 6–7, spec §7). Cards live
//! as age ciphertext in the vault ([`crypto`], [`store`]); the only place a
//! plaintext card exists is [`crypto::CardRecord`], constructed by the
//! registration CLI or by [`crypto::CardCipher::decrypt`] inside the engine's
//! approved fill path, and dropped as soon as the Camofox fill call returns.

pub mod artifacts;
pub mod auth;
pub mod crypto;
pub mod engine;
pub mod http;
pub mod payment;
pub mod ports;
pub mod state;
pub mod store;
pub mod wait;

pub use artifacts::{ARTIFACTS_DIR_ENV_VAR, ArtifactKind, DEFAULT_ARTIFACTS_DIR, ScreenshotStore};
pub use auth::{ApiKeys, AuthConfigError, AuthenticatedAgent};
pub use crypto::{CardCipher, CardFields, CardRecord, CryptoError};
pub use engine::{CreateOutcome, DecisionResult, EngineConfig, EngineError, PaymentEngine};
pub use http::{AppState, CreateResponse, router};
pub use payment::{
    CardId, CardSummary, Cart, CartItem, FieldKind, FieldSpec, InvalidCardId,
    InvalidOrderReference, NewPayment, OrderReference, Outcome, OutcomeKind, PaymentId,
    PaymentView, TerminalReason,
};
pub use ports::{
    ApprovalRequest, BoxFuture, CamofoxPort, DiscordPort, FillRequest, PortError,
    ResultReviewRequest,
};
pub use state::{IllegalTransition, PaymentEvent, PaymentState, transition};
pub use store::{MAX_PET_NAME_LEN, SqliteStore, StoreError, StoredPayment, TransitionEffect};
pub use wait::WaitRegistry;
