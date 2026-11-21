//! The agent-facing HTTP API (spec §5, DESIGN of till-1gt).
//!
//! Three endpoints, all behind a per-agent bearer key (ADR 0006):
//!
//! * `POST /payments` — create (or dedupe) a payment, returns its id.
//! * `GET /payments/{id}` — immediate read (US recovery path, D11).
//! * `GET /payments/{id}/wait?timeout=` — long-poll, returns on change or
//!   timeout with HTTP 200 (ADR 0004; the converged design chose `GET`, which
//!   is the erratum recorded against ADR 0004).
//!
//! Every response body is a [`PaymentView`] or a small scalar envelope, so no
//! endpoint can return a card value (AGENTS.md rule 6). A payment is also only
//! visible to the agent that created it: another agent's id reads as 404.

use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::auth::{ApiKeys, AuthenticatedAgent};
use crate::engine::{EngineError, PaymentEngine};
use crate::payment::{NewPayment, PaymentId, PaymentView};
use crate::state::PaymentState;
use crate::store::StoreError;

/// What the router needs: the engine and the configured agent keys.
#[derive(Clone, Debug)]
pub struct AppState {
    engine: PaymentEngine,
    keys: ApiKeys,
}

impl AppState {
    /// Assemble state from an engine and a key set.
    #[must_use]
    pub fn new(engine: PaymentEngine, keys: ApiKeys) -> Self {
        Self { engine, keys }
    }

    /// The engine behind this state.
    #[must_use]
    pub fn engine(&self) -> &PaymentEngine {
        &self.engine
    }

    /// The configured agent keys.
    #[must_use]
    pub fn api_keys(&self) -> &ApiKeys {
        &self.keys
    }
}

/// Build the agent-facing router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/payments", post(create_payment))
        .route("/payments/{payment_id}", get(get_payment))
        .route("/payments/{payment_id}/wait", get(wait_payment))
        .with_state(state)
}

/// The body of a successful `POST /payments` (US-1, US-13).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CreateResponse {
    /// The payment id, minted or deduped.
    pub payment_id: PaymentId,
    /// `false` when this POST matched an existing `idempotency_key`.
    pub created: bool,
    /// The payment's state right now (`pending_approval` on success).
    pub state: PaymentState,
}

async fn create_payment(
    State(state): State<AppState>,
    AuthenticatedAgent(agent_id): AuthenticatedAgent,
    Json(request): Json<NewPayment>,
) -> Result<(StatusCode, Json<CreateResponse>), ApiError> {
    let outcome = state.engine().create_payment(&agent_id, request).await?;
    let view = state.engine().view_for(&agent_id, outcome.payment_id)?;
    let status = if outcome.created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((
        status,
        Json(CreateResponse {
            payment_id: outcome.payment_id,
            created: outcome.created,
            state: view.state,
        }),
    ))
}

async fn get_payment(
    State(state): State<AppState>,
    AuthenticatedAgent(agent_id): AuthenticatedAgent,
    Path(payment_id): Path<String>,
) -> Result<Json<PaymentView>, ApiError> {
    let payment_id = parse_payment_id(&payment_id)?;
    Ok(Json(state.engine().view_for(&agent_id, payment_id)?))
}

#[derive(Debug, Deserialize)]
struct WaitParams {
    /// Requested budget in seconds; clamped to the server maximum (ADR 0004).
    timeout: Option<u64>,
}

async fn wait_payment(
    State(state): State<AppState>,
    AuthenticatedAgent(agent_id): AuthenticatedAgent,
    Path(payment_id): Path<String>,
    Query(params): Query<WaitParams>,
) -> Result<Json<PaymentView>, ApiError> {
    let payment_id = parse_payment_id(&payment_id)?;
    let timeout = params.timeout.map(Duration::from_secs);
    Ok(Json(
        state
            .engine()
            .wait_for(&agent_id, payment_id, timeout)
            .await?,
    ))
}

fn parse_payment_id(raw: &str) -> Result<PaymentId, ApiError> {
    raw.parse().map_err(|_| ApiError::not_found())
}

/// An API failure. Every message is static text, so nothing agent-supplied can
/// be reflected and no internal detail (let alone card data) can escape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
}

impl ApiError {
    /// 401 — no or invalid bearer key.
    #[must_use]
    pub fn unauthorized() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code: "unauthorized",
            message: "a valid agent API key is required",
        }
    }

    /// 404 — unknown payment, or one owned by another agent.
    #[must_use]
    pub fn not_found() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "not_found",
            message: "no such payment",
        }
    }

    /// 400 — unusable request.
    #[must_use]
    pub fn bad_request(message: &'static str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "invalid_request",
            message,
        }
    }

    /// 500 — a broker-side failure.
    #[must_use]
    pub fn internal() -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal",
            message: "the broker could not complete the request",
        }
    }

    /// The status this error answers with.
    #[must_use]
    pub fn status(self) -> StatusCode {
        self.status
    }
}

impl From<EngineError> for ApiError {
    fn from(error: EngineError) -> Self {
        match error {
            EngineError::Invalid(message) => Self::bad_request(message),
            EngineError::Store(StoreError::NotFound(_)) => Self::not_found(),
            EngineError::Store(error) => {
                // Ids and states only — this path can never see card data.
                eprintln!("broker: request failed: {error}");
                Self::internal()
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({ "error": { "code": self.code, "message": self.message } })),
        )
            .into_response()
    }
}
