//! Integration tests for the broker core: the state machine, the HTTP API, the
//! long-poll and idempotency (till-1gt acceptance criteria).
//!
//! Everything here runs in-process. Camofox and Discord are the [`Driver`]
//! below — a test double that records every fill/click/approval/review and can
//! be scripted to fail — so no network, no browser, no Discord is involved.
//!
//! The bodies contain obvious placeholder values only; card data does not exist
//! anywhere in this crate (see `crates/broker/src/payment.rs`).

use std::error::Error;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use broker::{
    ApiKeys, AppState, ApprovalRequest, BoxFuture, CamofoxPort, Cart, CartItem, DiscordPort,
    EngineConfig, EngineError, FieldKind, FieldSpec, FillRequest, NewPayment, OrderReference,
    Outcome, PaymentEngine, PaymentEvent, PaymentId, PaymentState, PaymentView, PortError,
    ResultReviewRequest, SqliteStore, StoreError, TransitionEffect,
};
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use tower::ServiceExt;

type TestResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

const AGENT: &str = "jamie";
const API_KEY: &str = "test-agent-key";

// ---------------------------------------------------------------------------
// Test double for the two ports
// ---------------------------------------------------------------------------

/// Records what the broker asked for, and fails on demand.
#[derive(Debug, Default)]
struct Driver {
    fills: Mutex<Vec<PaymentId>>,
    clicks: Mutex<Vec<PaymentId>>,
    approvals: Mutex<Vec<PaymentId>>,
    reviews: Mutex<Vec<PaymentId>>,
    fill_fails: AtomicBool,
    click_fails: AtomicBool,
}

impl Driver {
    fn fill_fails(mut self) -> Self {
        self.fill_fails = AtomicBool::new(true);
        self
    }

    fn click_fails(mut self) -> Self {
        self.click_fails = AtomicBool::new(true);
        self
    }

    fn record(slot: &Mutex<Vec<PaymentId>>, payment_id: PaymentId) {
        if let Ok(mut ids) = slot.lock() {
            ids.push(payment_id);
        }
    }

    fn fills(&self) -> usize {
        self.fills.lock().map(|ids| ids.len()).unwrap_or_default()
    }

    fn clicks(&self) -> usize {
        self.clicks.lock().map(|ids| ids.len()).unwrap_or_default()
    }

    fn approvals(&self) -> usize {
        self.approvals
            .lock()
            .map(|ids| ids.len())
            .unwrap_or_default()
    }

    fn reviews(&self) -> usize {
        self.reviews.lock().map(|ids| ids.len()).unwrap_or_default()
    }
}

impl CamofoxPort for Driver {
    fn fill_fields<'a>(&'a self, request: FillRequest<'a>) -> BoxFuture<'a, Result<(), PortError>> {
        Self::record(&self.fills, request.payment_id);
        let fails = self.fill_fails.load(Ordering::Relaxed);
        Box::pin(async move { if fails { Err(PortError) } else { Ok(()) } })
    }

    fn click_submit<'a>(
        &'a self,
        payment_id: PaymentId,
        _tab_id: &'a str,
        _submit_selector: &'a str,
    ) -> BoxFuture<'a, Result<(), PortError>> {
        Self::record(&self.clicks, payment_id);
        let fails = self.click_fails.load(Ordering::Relaxed);
        Box::pin(async move { if fails { Err(PortError) } else { Ok(()) } })
    }
}

impl DiscordPort for Driver {
    fn request_approval(
        &self,
        request: ApprovalRequest<'_>,
    ) -> BoxFuture<'_, Result<(), PortError>> {
        Self::record(&self.approvals, request.payment_id);
        Box::pin(async { Ok(()) })
    }

    fn request_result_review(
        &self,
        request: ResultReviewRequest<'_>,
    ) -> BoxFuture<'_, Result<(), PortError>> {
        Self::record(&self.reviews, request.payment_id);
        Box::pin(async { Ok(()) })
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Harness {
    engine: PaymentEngine,
    app: Router,
    driver: Arc<Driver>,
    path: PathBuf,
}

impl Harness {
    fn new(tag: &str, driver: Arc<Driver>, config: EngineConfig) -> TestResult<Self> {
        let path = temp_db_path(tag);
        let store = Arc::new(SqliteStore::open(&path)?);
        let engine = PaymentEngine::new(
            Arc::clone(&store),
            Arc::clone(&driver) as Arc<dyn CamofoxPort>,
            Arc::clone(&driver) as Arc<dyn DiscordPort>,
            config,
        );
        let app = broker::router(AppState::new(engine.clone(), keys()?));
        Ok(Self {
            engine,
            app,
            driver,
            path,
        })
    }

    /// Rebuild the engine over the same database file, as a restart would.
    fn restart(&self, config: EngineConfig) -> TestResult<PaymentEngine> {
        let store = Arc::new(SqliteStore::open(&self.path)?);
        Ok(PaymentEngine::new(
            Arc::clone(&store),
            Arc::clone(&self.driver) as Arc<dyn CamofoxPort>,
            Arc::clone(&self.driver) as Arc<dyn DiscordPort>,
            config,
        ))
    }

    async fn post(&self, key: &str, body: &NewPayment) -> TestResult<(StatusCode, Value)> {
        let request = Request::builder()
            .method("POST")
            .uri("/payments")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {key}"))
            .body(Body::from(serde_json::to_vec(body)?))?;
        collect(self.app.clone().oneshot(request).await?).await
    }

    async fn get(&self, key: &str, uri: &str) -> TestResult<(StatusCode, Value)> {
        let request = Request::builder()
            .uri(uri)
            .header("authorization", format!("Bearer {key}"))
            .body(Body::empty())?;
        collect(self.app.clone().oneshot(request).await?).await
    }

    async fn create(&self, body: &NewPayment) -> TestResult<PaymentId> {
        let (status, json) = self.post(API_KEY, body).await?;
        assert_eq!(status, StatusCode::CREATED, "POST /payments: {json}");
        let id = json["payment_id"].as_str().ok_or("missing payment_id")?;
        Ok(id.parse()?)
    }

    fn view(&self, payment_id: PaymentId) -> Result<PaymentView, EngineError> {
        self.engine.view(payment_id)
    }

    /// Poll until the payment reaches `want` (the fill pipeline runs in a task).
    async fn await_state(
        &self,
        payment_id: PaymentId,
        want: PaymentState,
    ) -> TestResult<PaymentView> {
        for _ in 0..500 {
            let view = self.view(payment_id)?;
            if view.state == want {
                return Ok(view);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let actual = self.view(payment_id)?.state;
        Err(format!("expected {want:?}, still {actual:?}").into())
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        remove_db(&self.path);
    }
}

/// Test cleanup only: removes this test's own database and WAL sidecars.
fn remove_db(path: &std::path::Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut file = path.as_os_str().to_owned();
        file.push(suffix);
        let _ = std::fs::remove_file(file);
    }
}

fn temp_db_path(tag: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("broker-test-{tag}-{}-{seq}.db", std::process::id()))
}

fn keys() -> TestResult<ApiKeys> {
    Ok(ApiKeys::from_spec(&format!("{AGENT}={API_KEY}"))?)
}

fn cart() -> Cart {
    Cart {
        merchant_origin: "https://shop.example".to_string(),
        items: vec![CartItem {
            name: "Ticket".to_string(),
            quantity: Some(1),
            unit_price: Some("42.00".to_string()),
        }],
        total: "42.00".to_string(),
        currency: "USD".to_string(),
    }
}

fn request(idempotency_key: &str) -> NewPayment {
    NewPayment {
        idempotency_key: idempotency_key.to_string(),
        tab_id: "tab-1".to_string(),
        cart: cart(),
        fields: vec![
            FieldSpec {
                selector: "#card".to_string(),
                field_kind: FieldKind::CardNumber,
            },
            FieldSpec {
                selector: "#cvv".to_string(),
                field_kind: FieldKind::Cvv,
            },
        ],
        submit_selector: "#pay".to_string(),
    }
}

async fn collect(response: axum::response::Response) -> TestResult<(StatusCode, Value)> {
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024).await?;
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)?
    };
    Ok((status, value))
}

/// Drive a payment to `submitting` with the latch committed, bypassing the
/// ports — the state a crash would leave behind (US-12).
fn seed_submitting(store: &SqliteStore, request: &NewPayment) -> TestResult<PaymentId> {
    let payment_id = store.insert_pending(AGENT, request, i64::MAX)?;
    for (from, event, effect) in [
        (
            PaymentState::PendingApproval,
            PaymentEvent::Approve,
            TransitionEffect::default(),
        ),
        (
            PaymentState::Approved,
            PaymentEvent::FillStarted,
            TransitionEffect::default(),
        ),
        (
            PaymentState::Filling,
            PaymentEvent::SubmitAttempted,
            TransitionEffect {
                submit_attempted: true,
                ..TransitionEffect::default()
            },
        ),
    ] {
        store.transition(payment_id, from, event, &effect)?;
    }
    Ok(payment_id)
}

// ---------------------------------------------------------------------------
// Creation, idempotency, auth
// ---------------------------------------------------------------------------

#[tokio::test]
async fn post_payments_creates_pending_approval() -> TestResult<()> {
    let harness = Harness::new(
        "create",
        Arc::new(Driver::default()),
        EngineConfig::default(),
    )?;
    let (status, body) = harness.post(API_KEY, &request("key-1")).await?;

    assert_eq!(status, StatusCode::CREATED, "POST /payments: {body}");
    assert_eq!(body["state"], json!("pending_approval"));
    assert_eq!(body["created"], json!(true));
    assert_eq!(harness.driver.approvals(), 1, "the human must be asked");
    Ok(())
}

#[tokio::test]
async fn idempotency_key_dedupe_returns_same_payment_id() -> TestResult<()> {
    let harness = Harness::new(
        "dedupe",
        Arc::new(Driver::default()),
        EngineConfig::default(),
    )?;

    let (first_status, first) = harness.post(API_KEY, &request("same-key")).await?;
    let (second_status, second) = harness.post(API_KEY, &request("same-key")).await?;

    assert_eq!(first_status, StatusCode::CREATED);
    assert_eq!(second_status, StatusCode::OK, "a dedupe is not a creation");
    assert_eq!(
        first["payment_id"], second["payment_id"],
        "US-13: one payment"
    );
    assert_eq!(second["created"], json!(false));
    Ok(())
}

#[tokio::test]
async fn idempotency_dedupe_survives_restart() -> TestResult<()> {
    let harness = Harness::new(
        "dedupe-restart",
        Arc::new(Driver::default()),
        EngineConfig::default(),
    )?;
    let payment_id = harness.create(&request("restart-key")).await?;

    // A second store over the same file stands in for a broker restart.
    let restarted = harness.restart(EngineConfig::default())?;
    let again = restarted
        .create_payment(AGENT, request("restart-key"))
        .await?;

    assert_eq!(
        again.payment_id, payment_id,
        "the unique index outlives the process"
    );
    assert!(!again.created, "a restart must not mint a second payment");
    Ok(())
}

#[tokio::test]
async fn missing_or_wrong_api_key_is_401() -> TestResult<()> {
    let harness = Harness::new("auth", Arc::new(Driver::default()), EngineConfig::default())?;
    let payment_id = harness.create(&request("auth-key")).await?;

    let (bad_post, _) = harness.post("wrong-key", &request("auth-key-2")).await?;
    let (no_get, _) = harness
        .get("wrong-key", &format!("/payments/{payment_id}"))
        .await?;
    let (no_wait, _) = harness
        .get(
            "wrong-key",
            &format!("/payments/{payment_id}/wait?timeout=1"),
        )
        .await?;

    assert_eq!(bad_post, StatusCode::UNAUTHORIZED);
    assert_eq!(no_get, StatusCode::UNAUTHORIZED);
    assert_eq!(no_wait, StatusCode::UNAUTHORIZED);
    Ok(())
}

// ---------------------------------------------------------------------------
// The happy path and the two refusals
// ---------------------------------------------------------------------------

#[tokio::test]
async fn approve_walks_filling_submitting_to_awaiting_result_review() -> TestResult<()> {
    let harness = Harness::new(
        "approve",
        Arc::new(Driver::default()),
        EngineConfig::default(),
    )?;
    let payment_id = harness.create(&request("approve-key")).await?;

    assert_eq!(
        harness.engine.approve(payment_id)?,
        broker::DecisionResult::Applied
    );
    let view = harness
        .await_state(payment_id, PaymentState::AwaitingResultReview)
        .await?;

    assert_eq!(view.state, PaymentState::AwaitingResultReview);
    assert_eq!(
        harness.driver.fills(),
        1,
        "one fill for the approved payment"
    );
    assert_eq!(harness.driver.clicks(), 1, "one click, exactly once");
    assert_eq!(harness.driver.reviews(), 1, "US-10: the human is asked");
    assert!(harness.engine.store().load(payment_id)?.submit_attempted);
    Ok(())
}

#[tokio::test]
async fn submit_click_issued_exactly_once() -> TestResult<()> {
    let harness = Harness::new("once", Arc::new(Driver::default()), EngineConfig::default())?;
    let payment_id = harness.create(&request("once-key")).await?;
    harness.engine.approve(payment_id)?;
    harness
        .await_state(payment_id, PaymentState::AwaitingResultReview)
        .await?;

    // A replayed Approve click must not click again (spec §6 stale clicks).
    let replayed = harness.engine.approve(payment_id)?;

    assert_eq!(replayed, broker::DecisionResult::Expired);
    assert_eq!(harness.driver.clicks(), 1, "the submit latch holds");
    Ok(())
}

#[tokio::test]
async fn decline_never_touches_camofox() -> TestResult<()> {
    let harness = Harness::new(
        "decline",
        Arc::new(Driver::default()),
        EngineConfig::default(),
    )?;
    let payment_id = harness.create(&request("decline-key")).await?;

    let decision = harness.engine.decline(payment_id)?;
    let view = harness.view(payment_id)?;

    assert_eq!(decision, broker::DecisionResult::Applied);
    assert_eq!(view.state, PaymentState::Declined, "US-8");
    assert_eq!(harness.driver.fills(), 0, "nothing may be filled");
    assert_eq!(harness.driver.clicks(), 0, "nothing may be submitted");
    Ok(())
}

// ---------------------------------------------------------------------------
// Hold expiry and late clicks (US-9)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hold_expiry_transitions_to_approval_timeout() -> TestResult<()> {
    let config = EngineConfig::new(Duration::from_secs(1), Duration::from_secs(120));
    let harness = Harness::new("ttl", Arc::new(Driver::default()), config)?;
    let payment_id = harness.create(&request("ttl-key")).await?;

    let view = harness
        .await_state(payment_id, PaymentState::ApprovalTimeout)
        .await?;

    assert_eq!(view.state, PaymentState::ApprovalTimeout);
    assert_eq!(
        view.terminal_reason,
        Some(broker::TerminalReason::HoldExpired)
    );
    assert_eq!(harness.driver.fills(), 0, "no approval, no fill");
    Ok(())
}

#[tokio::test]
async fn late_approve_after_timeout_is_rejected_nothing_filled() -> TestResult<()> {
    let config = EngineConfig::new(Duration::from_secs(1), Duration::from_secs(120));
    let harness = Harness::new("late", Arc::new(Driver::default()), config)?;
    let payment_id = harness.create(&request("late-key")).await?;
    harness
        .await_state(payment_id, PaymentState::ApprovalTimeout)
        .await?;

    let decision = harness.engine.approve(payment_id)?;

    assert_eq!(decision, broker::DecisionResult::Expired, "US-9");
    assert_eq!(harness.driver.fills(), 0, "a late Approve fills nothing");
    assert_eq!(harness.driver.clicks(), 0, "a late Approve submits nothing");
    Ok(())
}

#[tokio::test]
async fn late_decline_after_timeout_closes_as_approval_timeout() -> TestResult<()> {
    let config = EngineConfig::new(Duration::from_secs(1), Duration::from_secs(120));
    let harness = Harness::new("late-decline", Arc::new(Driver::default()), config)?;
    let payment_id = harness.create(&request("late-decline-key")).await?;
    harness
        .await_state(payment_id, PaymentState::ApprovalTimeout)
        .await?;

    let decision = harness.engine.decline(payment_id)?;

    assert_eq!(
        decision,
        broker::DecisionResult::Expired,
        "both buttons are late after the hold"
    );
    assert_eq!(
        harness.view(payment_id)?.state,
        PaymentState::ApprovalTimeout,
        "a late Decline must not rewrite the close reason"
    );
    assert_eq!(harness.driver.fills(), 0);
    Ok(())
}

// ---------------------------------------------------------------------------
// Pre-submit and post-submit failures
// ---------------------------------------------------------------------------

#[tokio::test]
async fn camofox_fill_error_aborts_before_submit() -> TestResult<()> {
    let driver = Arc::new(Driver::default().fill_fails());
    let harness = Harness::new("fill-fail", driver, EngineConfig::default())?;
    let payment_id = harness.create(&request("fill-fail-key")).await?;
    harness.engine.approve(payment_id)?;

    let view = harness
        .await_state(payment_id, PaymentState::AbortedBeforeSubmit)
        .await?;

    assert_eq!(view.state, PaymentState::AbortedBeforeSubmit);
    assert_eq!(harness.driver.clicks(), 0, "a fill error must not click");
    assert!(!harness.engine.store().load(payment_id)?.submit_attempted);
    Ok(())
}

#[tokio::test]
async fn click_disconnect_yields_ambiguous_never_retry() -> TestResult<()> {
    let driver = Arc::new(Driver::default().click_fails());
    let harness = Harness::new("click-fail", driver, EngineConfig::default())?;
    let payment_id = harness.create(&request("click-fail-key")).await?;
    harness.engine.approve(payment_id)?;

    let view = harness
        .await_state(payment_id, PaymentState::Ambiguous)
        .await?;

    assert_eq!(view.state, PaymentState::Ambiguous, "spec §7: never retry");
    assert!(
        harness.engine.store().load(payment_id)?.submit_attempted,
        "the latch was committed before the click"
    );
    assert_eq!(harness.driver.clicks(), 1);
    Ok(())
}

#[tokio::test]
async fn crash_recovery_marks_inflight_submitting_ambiguous() -> TestResult<()> {
    let path = temp_db_path("recovery");
    let store = Arc::new(SqliteStore::open(&path)?);
    let payment_id = seed_submitting(&store, &request("recovery-key"))?;

    // A fresh process finds the row in `submitting` and cannot prove the click
    // landed, so it must go ambiguous rather than retry (US-12, ADR 0001).
    let driver = Arc::new(Driver::default());
    let engine = PaymentEngine::new(
        Arc::clone(&store),
        Arc::clone(&driver) as Arc<dyn CamofoxPort>,
        Arc::clone(&driver) as Arc<dyn DiscordPort>,
        EngineConfig::default(),
    );
    engine.recover()?;
    let view = engine.view(payment_id)?;

    assert_eq!(view.state, PaymentState::Ambiguous);
    assert_eq!(
        view.terminal_reason,
        Some(broker::TerminalReason::CrashRecovery)
    );
    assert_eq!(driver.clicks(), 0, "recovery never clicks");
    remove_db(&path);
    Ok(())
}

#[tokio::test]
async fn crash_recovery_aborts_presubmit_and_expires_stale_holds() -> TestResult<()> {
    let path = temp_db_path("recovery-presubmit");
    let store = Arc::new(SqliteStore::open(&path)?);
    // `filling` with no latch: the process died before any click.
    let filling = store.insert_pending(AGENT, &request("recovery-filling"), i64::MAX)?;
    store.transition(
        filling,
        PaymentState::PendingApproval,
        PaymentEvent::Approve,
        &TransitionEffect::default(),
    )?;
    store.transition(
        filling,
        PaymentState::Approved,
        PaymentEvent::FillStarted,
        &TransitionEffect::default(),
    )?;
    // `pending_approval` whose hold already expired while nobody was running.
    let expired = store.insert_pending(AGENT, &request("recovery-expired"), 0)?;

    let driver = Arc::new(Driver::default());
    let engine = PaymentEngine::new(
        Arc::clone(&store),
        Arc::clone(&driver) as Arc<dyn CamofoxPort>,
        Arc::clone(&driver) as Arc<dyn DiscordPort>,
        EngineConfig::default(),
    );
    engine.recover()?;

    assert_eq!(
        engine.view(filling)?.state,
        PaymentState::AbortedBeforeSubmit,
        "pre-submit crash: nothing was clicked"
    );
    assert_eq!(
        engine.view(expired)?.state,
        PaymentState::ApprovalTimeout,
        "an expired hold does not survive a restart"
    );
    remove_db(&path);
    Ok(())
}

// ---------------------------------------------------------------------------
// Outcomes and stale interactions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn outcome_buttons_map_to_terminal_states() -> TestResult<()> {
    let harness = Harness::new(
        "outcomes",
        Arc::new(Driver::default()),
        EngineConfig::default(),
    )?;

    let mut reached = Vec::new();
    for (index, outcome) in [
        Outcome::Succeeded {
            order_reference: Some(OrderReference::parse("MC-000123")?),
        },
        Outcome::FailedNoCharge,
        Outcome::Ambiguous,
    ]
    .into_iter()
    .enumerate()
    {
        let payment_id = harness
            .create(&request(&format!("outcome-key-{index}")))
            .await?;
        harness.engine.approve(payment_id)?;
        harness
            .await_state(payment_id, PaymentState::AwaitingResultReview)
            .await?;
        let decision = harness.engine.report_outcome(payment_id, outcome)?;
        assert_eq!(decision, broker::DecisionResult::Applied);
        reached.push(harness.view(payment_id)?.state);
    }

    assert_eq!(
        reached,
        vec![
            PaymentState::Succeeded,
            PaymentState::FailedNoCharge,
            PaymentState::Ambiguous
        ],
        "each button has its own terminal state (spec §6)"
    );
    Ok(())
}

#[tokio::test]
async fn stale_interaction_on_terminal_payment_rejected() -> TestResult<()> {
    let harness = Harness::new(
        "stale",
        Arc::new(Driver::default()),
        EngineConfig::default(),
    )?;
    let payment_id = harness.create(&request("stale-key")).await?;
    harness.engine.decline(payment_id)?;

    let before = harness.view(payment_id)?;
    let decision = harness.engine.report_outcome(
        payment_id,
        Outcome::Succeeded {
            order_reference: None,
        },
    )?;
    let after = harness.view(payment_id)?;

    assert_eq!(decision, broker::DecisionResult::Expired);
    assert_eq!(after.state, before.state, "a settled payment stays settled");
    Ok(())
}

// ---------------------------------------------------------------------------
// Long-poll (ADR 0004)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn wait_wakes_on_state_change() -> TestResult<()> {
    let harness = Harness::new(
        "wait-wake",
        Arc::new(Driver::default()),
        EngineConfig::default(),
    )?;
    let payment_id = harness.create(&request("wait-key")).await?;

    let waiting = {
        let engine = harness.engine.clone();
        tokio::spawn(async move { engine.wait(payment_id, Some(Duration::from_secs(30))).await })
    };
    // Give the waiter a moment to park on the channel, then move the machine.
    tokio::time::sleep(Duration::from_millis(50)).await;
    harness.engine.decline(payment_id)?;

    let view = join(waiting).await?;
    assert_eq!(
        view.state,
        PaymentState::Declined,
        "the wake carries the change"
    );
    Ok(())
}

#[tokio::test]
async fn wait_timeout_returns_current_non_terminal_state_with_200() -> TestResult<()> {
    let harness = Harness::new(
        "wait-timeout",
        Arc::new(Driver::default()),
        EngineConfig::default(),
    )?;
    let payment_id = harness.create(&request("wait-timeout-key")).await?;

    let started = std::time::Instant::now();
    let (status, body) = harness
        .get(API_KEY, &format!("/payments/{payment_id}/wait?timeout=1"))
        .await?;

    assert_eq!(status, StatusCode::OK, "a timeout is 200, not an error");
    assert_eq!(body["state"], json!("pending_approval"));
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the wait was bounded"
    );
    Ok(())
}

#[tokio::test]
async fn wait_returns_immediately_on_a_terminal_payment() -> TestResult<()> {
    let harness = Harness::new(
        "wait-terminal",
        Arc::new(Driver::default()),
        EngineConfig::default(),
    )?;
    let payment_id = harness.create(&request("wait-terminal-key")).await?;
    harness.engine.decline(payment_id)?;

    let (status, body) = harness
        .get(API_KEY, &format!("/payments/{payment_id}/wait?timeout=30"))
        .await?;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["state"], json!("declined"));
    Ok(())
}

async fn join(handle: JoinHandle<Result<PaymentView, EngineError>>) -> TestResult<PaymentView> {
    match handle.await {
        Ok(result) => Ok(result?),
        Err(error) => Err(error.into()),
    }
}

// ---------------------------------------------------------------------------
// Security boundary (AGENTS.md rules 6–7) and history
// ---------------------------------------------------------------------------

#[tokio::test]
async fn api_responses_never_contain_card_value_fields() -> TestResult<()> {
    let harness = Harness::new(
        "boundary",
        Arc::new(Driver::default()),
        EngineConfig::default(),
    )?;
    let payment_id = harness.create(&request("boundary-key")).await?;

    let (status, body) = harness
        .get(API_KEY, &format!("/payments/{payment_id}"))
        .await?;
    assert_eq!(status, StatusCode::OK);

    let mut keys = Vec::new();
    collect_keys(&body, &mut keys);
    let forbidden = [
        "card_number",
        "pan",
        "cvv",
        "expiry",
        "cardholder_name",
        "billing_postcode",
    ];
    for key in &keys {
        assert!(
            !forbidden.contains(&key.as_str()),
            "API response leaked a card field name: {key}"
        );
    }
    // No 13–19 digit run anywhere: neither a PAN nor a last-four may appear.
    let serialized = serde_json::to_string(&body)?;
    assert!(
        !contains_long_digit_run(&serialized),
        "API response contains a card-shaped digit run"
    );
    Ok(())
}

#[tokio::test]
async fn every_legal_transition_is_recorded_in_history() -> TestResult<()> {
    let harness = Harness::new(
        "history",
        Arc::new(Driver::default()),
        EngineConfig::default(),
    )?;
    let payment_id = harness.create(&request("history-key")).await?;
    harness.engine.approve(payment_id)?;
    harness
        .await_state(payment_id, PaymentState::AwaitingResultReview)
        .await?;
    harness.engine.report_outcome(
        payment_id,
        Outcome::Succeeded {
            order_reference: Some(OrderReference::parse("MC-42")?),
        },
    )?;

    let history = harness.engine.store().history(payment_id)?;
    let states: Vec<PaymentState> = history.iter().map(|(_, to, _)| *to).collect();

    assert_eq!(
        states,
        vec![
            PaymentState::PendingApproval,
            PaymentState::Approved,
            PaymentState::Filling,
            PaymentState::Submitting,
            PaymentState::AwaitingResultReview,
            PaymentState::Succeeded,
        ],
        "the append-only log is the product's history (ADR 0001)"
    );
    Ok(())
}

#[tokio::test]
async fn store_rejects_a_foreign_state_change() -> TestResult<()> {
    let harness = Harness::new("cas", Arc::new(Driver::default()), EngineConfig::default())?;
    let payment_id = harness.create(&request("cas-key")).await?;
    harness.engine.approve(payment_id)?;
    harness
        .await_state(payment_id, PaymentState::AwaitingResultReview)
        .await?;

    // The compare-and-set must refuse a transition from a state we are not in.
    let result = harness.engine.store().transition(
        payment_id,
        PaymentState::PendingApproval,
        PaymentEvent::Decline,
        &TransitionEffect::default(),
    );

    assert!(
        matches!(result, Err(StoreError::Conflict { .. })),
        "a stale from-state cannot move the machine"
    );
    Ok(())
}

/// The store is the single writer of `payments.state` and refuses any pair the
/// spec §4 table does not list — `pending_approval → filling` would fill a card
/// before approval (AGENTS.md rule 7), so it must be impossible to ask for.
#[tokio::test]
async fn store_refuses_a_transition_the_spec_does_not_list() -> TestResult<()> {
    let harness = Harness::new(
        "illegal",
        Arc::new(Driver::default()),
        EngineConfig::default(),
    )?;
    let payment_id = harness.create(&request("illegal-key")).await?;

    // `FillStarted` is only legal from `approved`; from `pending_approval` it is
    // the pre-approval fill that rule 7 forbids.
    let illegal = harness.engine.store().transition(
        payment_id,
        PaymentState::PendingApproval,
        PaymentEvent::FillStarted,
        &TransitionEffect::default(),
    );
    // And a terminal payment accepts nothing at all.
    harness.engine.decline(payment_id)?;
    let after_terminal = harness.engine.store().transition(
        payment_id,
        PaymentState::Declined,
        PaymentEvent::Approve,
        &TransitionEffect::default(),
    );

    assert!(
        matches!(illegal, Err(StoreError::Illegal(_))),
        "the table, not the caller, decides what is legal"
    );
    assert!(
        matches!(after_terminal, Err(StoreError::Illegal(_))),
        "a terminal state accepts nothing (stale-click rejection)"
    );
    assert_eq!(
        harness.view(payment_id)?.state,
        PaymentState::Declined,
        "a refused transition writes nothing"
    );
    assert_eq!(harness.driver.fills(), 0, "nothing could have been filled");
    Ok(())
}

fn collect_keys(value: &Value, keys: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                keys.push(key.clone());
                collect_keys(child, keys);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_keys(item, keys);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn contains_long_digit_run(text: &str) -> bool {
    let mut run = 0;
    for character in text.chars() {
        if character.is_ascii_digit() {
            run += 1;
            if run >= 13 {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}
