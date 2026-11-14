//! Typed client for the Camofox browser REST API.
//!
//! Key endpoint facts (verified 2026-11-14 against
//! redf0x1/camofox-browser docs; re-verify against the live instance in
//! the Phase 0 spike — bead till-1 — before relying on them):
//!
//! - Tabs are scoped per `userId`: `POST /tabs` with `{userId, sessionKey, url?}`.
//! - `POST /tabs/:tabId/evaluate` executes JS (64KB max, 5s default timeout).
//!   Fill uses this, never `/type` (which emits text onto the event bus).
//! - `GET /tabs/:tabId/screenshot` captures the page.
//! - `POST /tabs/:tabId/click`, `/navigate`, `/wait`.
//! - Optional `CAMOFOX_API_KEY` auth; per-user rate limits (20 req/min default).
