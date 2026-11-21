//! HTTP surface of the mock checkout site (spec §9).
//!
//! `POST /checkout` deliberately never extracts its body: the form fields the
//! broker fills (`card_number`, `expiry`, …) are dropped the moment they
//! arrive, so there is no code path here that could log or persist them. All
//! the routing state we need (mode, hold token, TTL) travels in the query
//! string of the form's `action`.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Json, Redirect, Response};
use axum::routing::get;
use serde::Serialize;

use crate::Config;
use crate::mode::Mode;
use crate::pages;
use crate::stats::{StatsSnapshot, SubmissionCounter};

/// Query parameters, kept as a loose map so unknown ones are harmless and a
/// malformed numeric value can never turn a checkout into a 400.
type Params = BTreeMap<String, String>;

/// One live `short_hold` checkout session.
#[derive(Debug, Clone, Copy)]
struct Hold {
    opened: Instant,
    ttl: Duration,
}

impl Hold {
    fn expired(&self) -> bool {
        self.opened.elapsed() >= self.ttl
    }
}

/// Shared state for the router: config, counter and outstanding holds.
#[derive(Debug, Clone)]
pub struct AppState {
    config: Config,
    counter: Arc<SubmissionCounter>,
    holds: Arc<Mutex<BTreeMap<String, Hold>>>,
    token_seq: Arc<AtomicU64>,
}

impl AppState {
    /// Build state, opening the append-only counter file named by
    /// `config.stats_file`.
    pub fn new(config: Config) -> std::io::Result<Self> {
        let counter = SubmissionCounter::open(config.stats_file.clone())?;
        Ok(Self {
            config,
            counter: Arc::new(counter),
            holds: Arc::new(Mutex::new(BTreeMap::new())),
            token_seq: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Configuration this state was built from.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The submission counter behind `/stats`.
    pub fn counter(&self) -> &SubmissionCounter {
        &self.counter
    }

    fn mode_for(&self, params: &Params) -> Mode {
        self.config.mode_for(params.get("mode").map(String::as_str))
    }

    fn hold_seconds_for(&self, params: &Params) -> u64 {
        params
            .get("hold_seconds")
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(self.config.hold_seconds)
    }

    fn new_token(&self) -> String {
        let seq = self.token_seq.fetch_add(1, Ordering::Relaxed);
        format!("hold-{seq}-{}", self.counter.total())
    }

    fn register_hold(&self, token: &str, hold_seconds: u64) {
        let hold = Hold {
            opened: Instant::now(),
            ttl: Duration::from_secs(hold_seconds),
        };
        if let Ok(mut holds) = self.holds.lock() {
            holds.insert(token.to_string(), hold);
        }
    }

    /// Consume a hold: holds are single-use, so a replayed token is expired.
    fn take_hold(&self, token: &str) -> Option<Hold> {
        self.holds
            .lock()
            .ok()
            .and_then(|mut holds| holds.remove(token))
    }
}

/// Response body of `GET /stats`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatsResponse {
    /// Total accepted submissions, cumulative across restarts.
    pub total: u64,
    /// Per-mode totals, keyed by wire name.
    pub by_mode: BTreeMap<String, u64>,
}

impl From<StatsSnapshot> for StatsResponse {
    fn from(snapshot: StatsSnapshot) -> Self {
        Self {
            total: snapshot.total,
            by_mode: snapshot.by_mode,
        }
    }
}

/// Build the site's router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/checkout", get(show_checkout).post(submit_checkout))
        .route("/stats", get(stats))
        .with_state(state)
}

async fn root() -> Redirect {
    Redirect::temporary("/checkout")
}

async fn show_checkout(State(state): State<AppState>, Query(params): Query<Params>) -> Response {
    let mode = state.mode_for(&params);
    let action = match mode {
        Mode::ShortHold => {
            let hold_seconds = state.hold_seconds_for(&params);
            let token = params
                .get("token")
                .filter(|token| is_url_safe_token(token))
                .cloned()
                .unwrap_or_else(|| state.new_token());
            state.register_hold(&token, hold_seconds);
            format!("/checkout?mode={mode}&token={token}&hold_seconds={hold_seconds}")
        }
        _ => format!("/checkout?mode={mode}"),
    };

    let hold_notice = (mode == Mode::ShortHold).then(|| {
        format!(
            "This checkout expires {} seconds after it was opened.",
            state.hold_seconds_for(&params)
        )
    });

    Html(pages::checkout_form(mode, &action, hold_notice.as_deref())).into_response()
}

async fn submit_checkout(State(state): State<AppState>, Query(params): Query<Params>) -> Response {
    let mode = state.mode_for(&params);

    if mode == Mode::ShortHold {
        // Holds are single-use: `take_hold` removes the session, so a replayed
        // or unknown token reads as expired.
        let hold = params.get("token").and_then(|token| state.take_hold(token));
        let (accepted, hold_seconds) = match hold {
            Some(hold) => (!hold.expired(), hold.ttl.as_secs()),
            None => (false, state.hold_seconds_for(&params)),
        };
        if !accepted {
            // The click may have landed, but the merchant-side session was
            // gone: nothing was submitted, so nothing is counted.
            return (
                StatusCode::GONE,
                Html(pages::checkout_expired(hold_seconds)),
            )
                .into_response();
        }
    }

    if let Err(error) = state.counter.record(mode) {
        // io::Error only — there is no card data anywhere near this path.
        eprintln!("mock-checkout: failed to record submission: {error}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html(pages::internal_error("could not record the submission")),
        )
            .into_response();
    }

    let page = match mode {
        Mode::Success | Mode::ShortHold => Html(pages::order_confirmed(&order_reference(
            state.counter.total(),
        ))),
        Mode::Declined => Html(pages::card_declined()),
        Mode::Ambiguous => Html(pages::processing()),
    };
    page.into_response()
}

async fn stats(State(state): State<AppState>) -> Json<StatsResponse> {
    Json(StatsResponse::from(state.counter().snapshot()))
}

fn order_reference(total: u64) -> String {
    format!("MC-{total:06}")
}

fn is_url_safe_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 64
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}
