//! Integration tests for the supervised fill → submit → mask → capture
//! pipeline (bead till-ipx; ADR 0005, ADR 0008).
//!
//! These tests use the engine's own ports, so no browser and no Discord are
//! involved — the client's wire behaviour is covered in
//! `crates/camofox-client/tests/fill_engine.rs`. What is asserted here is the
//! *ordering and durability* the engine is responsible for:
//!
//! * the masked artifact lands in the store, keyed by payment id (the handoff
//!   to the Discord gateway, till-q1k);
//! * a capture failure cannot un-prove a click that already landed;
//! * a fill failure never clicks and never captures;
//! * containment runs exactly once per settled payment, and never before;
//! * a restart never re-fills or re-clicks a latched payment ("exactly one
//!   submit per payment, including across broker restarts" — SPEC §9).

use std::error::Error;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use broker::{
    ApprovalRequest, ArtifactKind, BoxFuture, CamofoxPort, CardCipher, CardRecord, Cart, CartItem,
    DiscordPort, EngineConfig, FieldKind, FieldSpec, FillRequest, NewPayment, Outcome,
    PaymentEngine, PaymentEvent, PaymentId, PaymentState, PortError, ResultReviewRequest,
    ScreenshotStore, SqliteStore, TransitionEffect,
};

type TestResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

const AGENT: &str = "jamie";

// ---------------------------------------------------------------------------
// Test double
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Call {
    Fill,
    Click,
    Settle,
    Capture,
    Contain,
}

#[derive(Debug, Default)]
struct Driver {
    calls: Mutex<Vec<Call>>,
    fill_fails: AtomicBool,
    click_fails: AtomicBool,
    capture_fails: AtomicBool,
}

impl Driver {
    fn failing(fill: bool, click: bool, capture: bool) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            fill_fails: AtomicBool::new(fill),
            click_fails: AtomicBool::new(click),
            capture_fails: AtomicBool::new(capture),
        })
    }

    fn record(&self, call: Call) {
        if let Ok(mut calls) = self.calls.lock() {
            calls.push(call);
        }
    }

    fn count(&self, call: Call) -> usize {
        self.calls
            .lock()
            .map(|calls| calls.iter().filter(|seen| **seen == call).count())
            .unwrap_or_default()
    }

    fn sequence(&self) -> Vec<Call> {
        self.calls
            .lock()
            .map(|calls| calls.clone())
            .unwrap_or_default()
    }
}

/// A one-byte-body PNG stand-in: the engine must pass the bytes through
/// untouched, and no test needs real pixels.
fn png() -> Vec<u8> {
    vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]
}

impl CamofoxPort for Driver {
    fn fill_fields<'a>(
        &'a self,
        _request: FillRequest<'a>,
    ) -> BoxFuture<'a, Result<(), PortError>> {
        self.record(Call::Fill);
        let fails = self.fill_fails.load(Ordering::Relaxed);
        Box::pin(async move { if fails { Err(PortError) } else { Ok(()) } })
    }

    fn click_submit<'a>(
        &'a self,
        _payment_id: PaymentId,
        _tab_id: &'a str,
        _submit_selector: &'a str,
    ) -> BoxFuture<'a, Result<(), PortError>> {
        self.record(Call::Click);
        let fails = self.click_fails.load(Ordering::Relaxed);
        Box::pin(async move { if fails { Err(PortError) } else { Ok(()) } })
    }

    fn wait_for_settle<'a>(
        &'a self,
        _payment_id: PaymentId,
        _tab_id: &'a str,
    ) -> BoxFuture<'a, Result<(), PortError>> {
        self.record(Call::Settle);
        Box::pin(async { Ok(()) })
    }

    fn capture_masked_result<'a>(
        &'a self,
        _payment_id: PaymentId,
        _tab_id: &'a str,
        _fields: &'a [FieldSpec],
    ) -> BoxFuture<'a, Result<Vec<u8>, PortError>> {
        self.record(Call::Capture);
        let fails = self.capture_fails.load(Ordering::Relaxed);
        Box::pin(async move { if fails { Err(PortError) } else { Ok(png()) } })
    }

    fn contain_tab<'a>(
        &'a self,
        _payment_id: PaymentId,
        _tab_id: &'a str,
    ) -> BoxFuture<'a, Result<(), PortError>> {
        self.record(Call::Contain);
        Box::pin(async { Ok(()) })
    }
}

impl DiscordPort for Driver {
    fn request_approval(
        &self,
        _request: ApprovalRequest<'_>,
    ) -> BoxFuture<'_, Result<(), PortError>> {
        Box::pin(async { Ok(()) })
    }

    fn request_result_review(
        &self,
        _request: ResultReviewRequest<'_>,
    ) -> BoxFuture<'_, Result<(), PortError>> {
        Box::pin(async { Ok(()) })
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Harness {
    engine: PaymentEngine,
    driver: Arc<Driver>,
    cipher: Arc<CardCipher>,
    store: Arc<SqliteStore>,
    artifacts: Arc<ScreenshotStore>,
    artifacts_dir: PathBuf,
    path: PathBuf,
}

impl Harness {
    fn new(tag: &str, driver: Arc<Driver>) -> TestResult<Self> {
        let path = temp_db_path(tag);
        let store = Arc::new(SqliteStore::open(&path)?);
        let cipher = Arc::new(CardCipher::generate());
        store.add_card(&cipher, "Test card", &test_card()?)?;
        let artifacts_dir = temp_artifacts_dir(tag);
        let artifacts = Arc::new(ScreenshotStore::open(&artifacts_dir)?);
        let engine = build_engine(
            Arc::clone(&store),
            Arc::clone(&driver),
            Arc::clone(&cipher),
            Arc::clone(&artifacts),
        );
        Ok(Self {
            engine,
            driver,
            cipher,
            store,
            artifacts,
            artifacts_dir,
            path,
        })
    }

    fn create(&self, key: &str) -> TestResult<PaymentId> {
        Ok(self
            .engine
            .store()
            .insert_pending(AGENT, &request(key), i64::MAX)?)
    }

    /// The same database and artifact store, but a fresh engine: a restart.
    fn restart(&self) -> PaymentEngine {
        build_engine(
            Arc::clone(&self.store),
            Arc::clone(&self.driver),
            Arc::clone(&self.cipher),
            Arc::clone(&self.artifacts),
        )
    }

    async fn await_state(&self, payment_id: PaymentId, want: PaymentState) -> TestResult<()> {
        for _ in 0..500 {
            if self.engine.view(payment_id)?.state == want {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let actual = self.engine.view(payment_id)?.state;
        Err(format!("expected {want:?}, still {actual:?}").into())
    }

    async fn await_containments(&self, want: usize) -> usize {
        for _ in 0..500 {
            let seen = self.driver.count(Call::Contain);
            if seen >= want {
                return seen;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        self.driver.count(Call::Contain)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        remove_db(&self.path);
        let _ = std::fs::remove_dir_all(&self.artifacts_dir);
    }
}

fn build_engine(
    store: Arc<SqliteStore>,
    driver: Arc<Driver>,
    cipher: Arc<CardCipher>,
    artifacts: Arc<ScreenshotStore>,
) -> PaymentEngine {
    PaymentEngine::new(
        store,
        Arc::clone(&driver) as Arc<dyn CamofoxPort>,
        Arc::clone(&driver) as Arc<dyn DiscordPort>,
        cipher,
        artifacts,
        EngineConfig::default(),
    )
}

fn temp_db_path(tag: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "broker-fill-pipeline-{tag}-{}-{seq}.db",
        std::process::id()
    ))
}

fn temp_artifacts_dir(tag: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "broker-fill-pipeline-artifacts-{tag}-{}-{seq}",
        std::process::id()
    ))
}

fn remove_db(path: &std::path::Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut file = path.as_os_str().to_owned();
        file.push(suffix);
        let _ = std::fs::remove_file(file);
    }
}

/// Synthetic values, built at runtime: no digit run ever appears in source
/// (`script/no-card-digits`, US-14).
fn test_card() -> TestResult<CardRecord> {
    Ok(CardRecord::parse(
        &"4".repeat(16),
        "12/34",
        &"1".repeat(3),
        "Test Holder",
        &"1".repeat(5),
    )?)
}

fn request(key: &str) -> NewPayment {
    NewPayment {
        idempotency_key: key.to_string(),
        tab_id: "tab-fill-pipeline".to_string(),
        cart: Cart {
            merchant_origin: "https://shop.example".to_string(),
            items: vec![CartItem {
                name: "Ticket".to_string(),
                quantity: Some(1),
                unit_price: Some("42.00".to_string()),
            }],
            total: "42.00".to_string(),
            currency: "USD".to_string(),
        },
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

// ---------------------------------------------------------------------------
// The artifact handoff (ADR 0008)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn result_artifact_written_keyed_by_payment_id_after_submit() -> TestResult<()> {
    let harness = Harness::new("artifact", Driver::failing(false, false, false))?;
    let payment_id = harness.create("artifact-key")?;

    harness.engine.approve(payment_id, None)?;
    harness
        .await_state(payment_id, PaymentState::AwaitingResultReview)
        .await?;

    let path = harness.artifacts.path(payment_id, ArtifactKind::Result);
    let stored = std::fs::read(&path)?;

    assert_eq!(stored, png(), "the engine stores the capture verbatim");
    assert_eq!(
        harness.driver.count(Call::Capture),
        1,
        "one masked capture per payment"
    );
    assert_eq!(
        harness.driver.count(Call::Settle),
        1,
        "the page settles before the capture"
    );
    // Ordering: settle, then mask+capture, and only after the click.
    let calls = harness.driver.sequence();
    let click_at = calls
        .iter()
        .position(|call| *call == Call::Click)
        .ok_or("no click")?;
    let settle_at = calls
        .iter()
        .position(|call| *call == Call::Settle)
        .ok_or("no settle")?;
    let capture_at = calls
        .iter()
        .position(|call| *call == Call::Capture)
        .ok_or("no capture")?;
    assert!(
        click_at < settle_at && settle_at < capture_at,
        "click → settle → capture, never another order: {calls:?}"
    );
    assert!(
        !calls.contains(&Call::Contain),
        "containment waits for the terminal outcome"
    );
    Ok(())
}

#[tokio::test]
async fn capture_failure_leaves_payment_in_awaiting_result_review() -> TestResult<()> {
    let harness = Harness::new("capture-fail", Driver::failing(false, false, true))?;
    let payment_id = harness.create("capture-fail-key")?;

    harness.engine.approve(payment_id, None)?;
    harness
        .await_state(payment_id, PaymentState::AwaitingResultReview)
        .await?;

    let stored = harness.engine.store().load(payment_id)?;
    assert_eq!(
        stored.state,
        PaymentState::AwaitingResultReview,
        "a capture failure cannot un-prove a click that landed"
    );
    assert!(stored.submit_attempted, "the latch still holds");
    assert_eq!(harness.driver.count(Call::Click), 1);
    assert!(
        !harness
            .artifacts
            .path(payment_id, ArtifactKind::Result)
            .exists(),
        "no artifact is written when the capture fails"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Failures before the click
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fill_failure_aborts_before_submit_and_writes_nothing() -> TestResult<()> {
    let harness = Harness::new("fill-fail", Driver::failing(true, false, false))?;
    let payment_id = harness.create("fill-fail-key")?;

    harness.engine.approve(payment_id, None)?;
    harness
        .await_state(payment_id, PaymentState::AbortedBeforeSubmit)
        .await?;

    assert_eq!(harness.driver.count(Call::Fill), 1);
    assert_eq!(harness.driver.count(Call::Click), 0, "nothing is submitted");
    assert_eq!(
        harness.driver.count(Call::Capture),
        0,
        "nothing is captured"
    );
    assert!(
        !harness
            .artifacts
            .path(payment_id, ArtifactKind::Result)
            .exists(),
        "no artifact for a payment that never submitted"
    );
    // A partially typed form still holds card values: the tab is contained.
    assert_eq!(
        harness.await_containments(1).await,
        1,
        "an aborted fill contains its tab"
    );
    Ok(())
}

#[tokio::test]
async fn click_failure_is_ambiguous_never_retried() -> TestResult<()> {
    let harness = Harness::new("click-fail", Driver::failing(false, true, false))?;
    let payment_id = harness.create("click-fail-key")?;

    harness.engine.approve(payment_id, None)?;
    harness
        .await_state(payment_id, PaymentState::Ambiguous)
        .await?;

    assert_eq!(
        harness.engine.view(payment_id)?.terminal_reason,
        Some(broker::TerminalReason::UnconfirmedSubmit)
    );
    assert_eq!(
        harness.driver.count(Call::Click),
        1,
        "one click attempt, never a retry"
    );
    assert_eq!(
        harness.driver.count(Call::Capture),
        0,
        "a payment we cannot prove landed is not captured"
    );
    assert_eq!(
        harness.await_containments(1).await,
        1,
        "an unconfirmed tab is contained"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Containment and the restart guarantee
// ---------------------------------------------------------------------------

#[tokio::test]
async fn containment_runs_exactly_once_after_outcome_click() -> TestResult<()> {
    let harness = Harness::new("contain", Driver::failing(false, false, false))?;
    let payment_id = harness.create("contain-key")?;

    harness.engine.approve(payment_id, None)?;
    harness
        .await_state(payment_id, PaymentState::AwaitingResultReview)
        .await?;
    assert_eq!(
        harness.driver.count(Call::Contain),
        0,
        "the tab is not released before the human settles the outcome"
    );

    harness
        .engine
        .report_outcome(payment_id, Outcome::FailedNoCharge)?;

    assert_eq!(
        harness.await_containments(1).await,
        1,
        "one terminal outcome contains its tab exactly once"
    );
    // A stale second click must not contain again.
    let _ = harness
        .engine
        .report_outcome(payment_id, Outcome::Ambiguous)?;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        harness.driver.count(Call::Contain),
        1,
        "a stale interaction never re-contains"
    );
    Ok(())
}

/// "Exactly one submit per payment, including across broker restarts"
/// (SPEC §9, acceptance criteria): a latched payment recovered by a fresh
/// process is `ambiguous` and is never filled or clicked again.
#[tokio::test]
async fn restart_never_refills_a_latched_payment() -> TestResult<()> {
    let harness = Harness::new("restart", Driver::failing(false, false, false))?;
    let payment_id = harness.create("restart-key")?;

    harness.engine.approve(payment_id, None)?;
    harness
        .await_state(payment_id, PaymentState::AwaitingResultReview)
        .await?;
    let fills_before = harness.driver.count(Call::Fill);
    let clicks_before = harness.driver.count(Call::Click);

    // Re-approve on the live engine: the latch must refuse.
    let _ = harness.engine.approve(payment_id, None)?;

    // Now a fresh process over the same database.
    let restarted = harness.restart();
    restarted.recover()?;
    let replay = restarted.approve(payment_id, None)?;

    assert_eq!(
        replay,
        broker::DecisionResult::Expired,
        "a latched payment refuses a replayed Approve"
    );
    assert_eq!(
        harness.driver.count(Call::Fill),
        fills_before,
        "a restart never fills again"
    );
    assert_eq!(
        harness.driver.count(Call::Click),
        clicks_before,
        "a restart never clicks again"
    );
    assert_eq!(
        harness.engine.view(payment_id)?.state,
        PaymentState::AwaitingResultReview,
        "recovery leaves a submitted payment waiting for its human"
    );
    Ok(())
}

/// A payment seeded in `submitting` (the exact state a crash leaves) is
/// recovered as `ambiguous`, contained, and never re-driven.
#[tokio::test]
async fn crash_recovery_contains_the_tab_and_never_replays() -> TestResult<()> {
    let harness = Harness::new("recovery", Driver::failing(false, false, false))?;
    let payment_id = harness
        .store
        .insert_pending(AGENT, &request("recovery-key"), i64::MAX)?;
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
        harness.store.transition(payment_id, from, event, &effect)?;
    }

    let restarted = harness.restart();
    restarted.recover()?;

    assert_eq!(
        restarted.view(payment_id)?.state,
        PaymentState::Ambiguous,
        "we cannot prove whether the click landed"
    );
    assert_eq!(
        harness.await_containments(1).await,
        1,
        "the recovered tab is contained"
    );
    assert_eq!(harness.driver.count(Call::Fill), 0);
    assert_eq!(harness.driver.count(Call::Click), 0);
    Ok(())
}
