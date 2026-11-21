//! Typed client for the Camofox browser REST API.
//!
//! Key endpoint facts (verified 2026-11-14 against
//! redf0x1/camofox-browser docs; re-verify against the live instance in
//! the Phase 0 spike — bead till-1 — before relying on them):
//!
//! - Tabs are scoped per `userId`: `POST /tabs` with `{userId, sessionKey, url?}`.
//! - `POST /tabs/:tabId/type` is the FILL primitive — on camofox ≥1.16
//!   (assumed; documented prerequisite, not runtime-checked) its plugin-bus
//!   payload is length-only (upstream #9436). `evaluate` is digit-free
//!   operations only: `about:blank` containment, field masking before
//!   screenshots (ADR 0005).
//! - `POST /tabs/:tabId/evaluate` executes JS (100KB max on 1.13, no timeout).
//! - `GET /tabs/:tabId/screenshot` captures the page.
//! - `POST /tabs/:tabId/click`, `/navigate`, `/wait`.
//! - Optional `CAMOFOX_API_KEY` auth; per-user rate limits (20 req/min default).
