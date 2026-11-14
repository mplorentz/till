//! Local mock checkout site (spec §9; bead till-2).
//!
//! Four programmable modes:
//! - `success`     → order reference on submit
//! - `declined`     → "card declined" error
//! - `ambiguous`    → indefinite "processing…" spinner
//! - `short_hold`   → 60-second checkout TTL
//!
//! Must expose a submission counter (`/stats`) so tests can assert the
//! submit button was clicked exactly once per payment — including across
//! broker restarts.
