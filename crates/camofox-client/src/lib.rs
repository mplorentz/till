//! Typed client and fill engine for the Camofox browser REST API (port 9377).
//!
//! Two layers, one policy:
//!
//! * [`client::CamofoxClient`] — typed HTTP methods for the endpoints the
//!   broker uses (`POST /tabs`, `/type`, `/evaluate`, `/click`, `/navigate`,
//!   `GET /screenshot`), scoped to one `userId`.
//! * [`fill::CamofoxFillEngine`] — the production [`broker::CamofoxPort`]: the
//!   supervised fill → submit → settle → mask → capture → contain sequence the
//!   broker's state machine drives (docs/SPEC.md §7).
//!
//! # Endpoint facts (verified 2026-11-14, docs/findings/camofox-spike.md)
//!
//! - Tabs are scoped per `userId`: `POST /tabs` with `{userId, sessionKey?, url?}`.
//! - `POST /tabs/:tabId/type` is the **fill primitive**. On camofox ≥ 1.16 its
//!   plugin-bus payload is length-only (upstream #9436).
//! - `POST /tabs/:tabId/evaluate` executes JS (100 KB body limit, no server
//!   timeout) and publishes **both** the expression and the result on the
//!   plugin event bus — hence digit-free use only.
//! - `GET /tabs/:tabId/screenshot` returns raw `image/png`.
//! - `POST /tabs/:tabId/click`, `/navigate`. `POST /tabs/:tabId/wait` exists but
//!   is never called (it ignores its selector and can click consent dialogs).
//! - `/navigate` rejects `about:blank`; containment uses `evaluate`.
//!
//! # Security rules this crate exists to keep
//!
//! Binding sources: AGENTS.md rules 1, 2, 4, 6, 7; SPEC §7; ADR 0005.
//!
//! 1. **Card values leave the broker through exactly one method.** The fill
//!    path is [`broker::CamofoxPort::fill_fields`]; inside this crate the only
//!    code that can reach `/type` is [`fill::CamofoxFillEngine`], because
//!    `CamofoxClient::type_into` is crate-private.
//! 2. **`/type`, never `evaluate`, for the fill** (ADR 0005 Decision 3). The
//!    `evaluate` parameter is a [`js::DigitFreeExpression`], a type with no
//!    public constructor, so an `evaluate` fill is not expressible.
//! 3. **`evaluate` carries no card digits** — only field *selectors*, which is
//!    what masking and containment need (ADR 0005 Decision 2).
//! 4. **One fill operation per payment.** One supervised pass, one `/type` per
//!    field, no retry (ADR 0005 Decision 2.1).
//! 5. **Mask before every capture.** The only method that returns pixels clears
//!    the payment fields first (US-10, AGENTS.md rule 4).
//! 6. **`about:blank` containment** after the payment settles (SPEC §7).
//! 7. **Playwright tracing is never enabled** by this client, and must stay
//!    off for payment tabs: traces write fill values to disk in plaintext
//!    (spike finding A).
//! 8. **The broker runs under the agent's `userId`** (spike Q3): tabs are
//!    strictly `userId`-scoped with no transfer API and `userId` carries no
//!    authentication. The residual DOM access during the fill→submit window is
//!    the accepted ADR 0005 exposure, fixed only by upstream per-principal auth
//!    plus tab leasing (ADR 0005 Tier 2). See [`config`].
//!
//! Camofox ≥ 1.16 is a **documented deployment prerequisite**, not a runtime
//! gate (overseer correction, 2026-11-21): the broker does not probe or refuse
//! a version, and ops keeps the host at ≥ 1.16 (SPEC §7, ADR 0005).

#![forbid(unsafe_code)]

pub mod client;
pub mod config;
pub mod fill;
pub mod js;

pub use client::{CamofoxClient, CamofoxError, REQUEST_TIMEOUT};
pub use config::{CamofoxConfig, ConfigError};
pub use fill::{CamofoxFillEngine, SettleConfig};
pub use js::DigitFreeExpression;
