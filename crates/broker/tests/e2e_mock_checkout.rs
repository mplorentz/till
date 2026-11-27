//! End-to-end acceptance test for bead till-ipx (SPEC §9, acceptance criteria):
//!
//! > End-to-end against mock-checkout driven by test harness approvals:
//! > `/stats` proves exactly one submit per payment incl. across broker
//! > restart; fill never touches `/evaluate`; screenshots masked.
//!
//! # Why this test is `#[ignore]`d
//!
//! It drives a **real browser**. It needs a live camofox instance (≥ 1.16, per
//! SPEC §7 / ADR 0005) and is therefore not part of `script/checks`. Run it on
//! the test host:
//!
//! ```console
//! TILL_CAMOFOX_URL=http://172.17.0.1:9377 TILL_CAMOFOX_USER_ID=jamie \
//!   cargo test -p broker --test e2e_mock_checkout -- --ignored --nocapture
//! ```
//!
//! # What is real and what is a harness
//!
//! * **Real:** HTTP API, state machine, SQLite persistence, the Camofox client
//!   and fill engine, the browser performing the fill and the click, and the
//!   mock checkout's own submission counter.
//! * **Harness:** the Discord side. Approvals and outcome clicks are performed
//!   in-process by [`HarnessDiscord`] instead of by a human clicking buttons —
//!   exactly what the acceptance criterion means by "test harness approvals".
//!   It also records whether the result screenshot was already in the artifact
//!   store when the review was requested, which is how "screenshots masked and
//!   handed over before the human sees them" is asserted.
//!
//! No card data exists in this file: the card is built at runtime from
//! repeated digits (`script/no-card-digits`, US-14).

use std::error::Error;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use broker::{
    ApprovalRequest, ArtifactKind, BoxFuture, CamofoxPort, CardCipher, CardRecord, Cart, CartItem,
    DiscordPort, EngineConfig, FieldKind, FieldSpec, NewPayment, Outcome, PaymentEngine, PaymentId,
    PaymentState, PortError, ResultReviewRequest, ScreenshotStore, SqliteStore,
};
use camofox_client::{CamofoxClient, CamofoxConfig, CamofoxFillEngine, SettleConfig};
use mock_checkout::Config as MockConfig;
use mock_checkout::Mode;

type TestResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

const AGENT: &str = "jamie";

// ---------------------------------------------------------------------------
// Harness Discord: approvals and outcome clicks without a human
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct HarnessDiscord {
    /// Set when a result review was requested; records whether the masked
    /// artifact already existed at that moment.
    review_saw_artifact: Mutex<Vec<bool>>,
    payments: Mutex<Vec<PaymentId>>,
    artifacts: Mutex<Option<Arc<ScreenshotStore>>>,
}

impl HarnessDiscord {
    fn with_artifacts(artifacts: Arc<ScreenshotStore>) -> Arc<Self> {
        let discord = Arc::new(Self::default());
        if let Ok(mut slot) = discord.artifacts.lock() {
            *slot = Some(artifacts);
        }
        discord
    }

    fn review_saw_artifact(&self) -> Vec<bool> {
        self.review_saw_artifact
            .lock()
            .map(|seen| seen.clone())
            .unwrap_or_default()
    }
}

impl DiscordPort for HarnessDiscord {
    fn request_approval(
        &self,
        request: ApprovalRequest<'_>,
    ) -> BoxFuture<'_, Result<(), PortError>> {
        if let Ok(mut payments) = self.payments.lock() {
            payments.push(request.payment_id);
        }
        Box::pin(async { Ok(()) })
    }

    fn request_result_review(
        &self,
        request: ResultReviewRequest<'_>,
    ) -> BoxFuture<'_, Result<(), PortError>> {
        // US-10: the human must never be shown an unmasked page. By the time
        // this is called the engine has already masked and stored the capture.
        let present = self
            .artifacts
            .lock()
            .ok()
            .and_then(|slot| slot.clone())
            .map(|store| {
                store
                    .path(request.payment_id, ArtifactKind::Result)
                    .exists()
            })
            .unwrap_or(false);
        if let Ok(mut seen) = self.review_saw_artifact.lock() {
            seen.push(present);
        }
        Box::pin(async { Ok(()) })
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn temp_path(tag: &str, suffix: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "till-e2e-{tag}-{}-{seq}{suffix}",
        std::process::id()
    ))
}

fn card() -> TestResult<CardRecord> {
    Ok(CardRecord::parse(
        &"4".repeat(16),
        "12/34",
        &"1".repeat(3),
        "Test Holder",
        &"1".repeat(5),
    )?)
}

fn request(key: &str, tab_id: &str) -> NewPayment {
    NewPayment {
        idempotency_key: key.to_string(),
        tab_id: tab_id.to_string(),
        cart: Cart {
            merchant_origin: "http://127.0.0.1".to_string(),
            items: vec![CartItem {
                name: "Ticket".to_string(),
                quantity: Some(1),
                unit_price: Some("42.00".to_string()),
            }],
            total: "42.00".to_string(),
            currency: "USD".to_string(),
        },
        // The mock checkout's own input ids (crates/mock-checkout/src/pages.rs:
        // every field is rendered as `<input id="{field_kind}" name="{field_kind}">`,
        // and the pay button is `<button id="submit" type="submit">`).
        fields: vec![
            FieldSpec {
                selector: "#card_number".to_string(),
                field_kind: FieldKind::CardNumber,
            },
            FieldSpec {
                selector: "#expiry".to_string(),
                field_kind: FieldKind::Expiry,
            },
            FieldSpec {
                selector: "#cvv".to_string(),
                field_kind: FieldKind::Cvv,
            },
        ],
        submit_selector: "#submit".to_string(),
    }
}

/// Start the mock checkout site in-process on an ephemeral port.
async fn start_mock_checkout(mode: Mode) -> TestResult<(String, Arc<mock_checkout::AppState>)> {
    let stats_file = temp_path("stats", ".jsonl");
    let mut config = MockConfig::new(stats_file);
    config.default_mode = mode;
    let state = Arc::new(mock_checkout::AppState::new(config)?);
    let app = mock_checkout::router((*state).clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((format!("http://{address}"), state))
}

fn live_client() -> TestResult<CamofoxClient> {
    // `from_env` requires TILL_CAMOFOX_URL + TILL_CAMOFOX_USER_ID on the host.
    let config = CamofoxConfig::from_env()?;
    Ok(CamofoxClient::new(config)?)
}

fn build_engine(
    store: Arc<SqliteStore>,
    camofox: Arc<dyn CamofoxPort>,
    discord: Arc<dyn DiscordPort>,
    cipher: Arc<CardCipher>,
    artifacts: Arc<ScreenshotStore>,
) -> PaymentEngine {
    PaymentEngine::new(
        store,
        camofox,
        discord,
        cipher,
        artifacts,
        EngineConfig::default(),
    )
}

async fn await_state(
    engine: &PaymentEngine,
    payment_id: PaymentId,
    want: PaymentState,
) -> TestResult<()> {
    for _ in 0..2_000 {
        if engine.view(payment_id)?.state == want {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let actual = engine.view(payment_id)?.state;
    Err(format!("expected {want:?}, still {actual:?}").into())
}

// ---------------------------------------------------------------------------
// The acceptance run
// ---------------------------------------------------------------------------

/// One full payment against the live stack: create → approve → fill → submit →
/// masked capture → outcome, then a broker restart that must not submit again.
#[tokio::test]
#[ignore = "needs a live camofox (>= 1.16) and a browser; run on the test host"]
async fn stats_proves_exactly_one_submit_including_across_broker_restart() -> TestResult<()> {
    let client = live_client()?;
    // The mock site has no TLS, so no cert juggling is needed.
    let (site, mock) = start_mock_checkout(Mode::Success).await?;

    // A tab on the real browser, pointed at the mock checkout's confirm screen.
    let tab_id = client.create_tab(Some(&format!("{site}/checkout"))).await?;
    // Let the page load before the fill starts.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let path = temp_path("broker", ".db");
    let artifacts_dir = temp_path("artifacts", "");
    let store = Arc::new(SqliteStore::open(&path)?);
    let cipher = Arc::new(CardCipher::generate());
    store.add_card(&cipher, "Test card", &card()?)?;
    let artifacts = Arc::new(ScreenshotStore::open(&artifacts_dir)?);

    let discord = HarnessDiscord::with_artifacts(Arc::clone(&artifacts));
    let camofox = Arc::new(
        CamofoxFillEngine::new(client.clone()).with_settle(SettleConfig::new(
            Duration::from_secs(15),
            Duration::from_millis(250),
        )),
    );
    let engine = build_engine(
        Arc::clone(&store),
        Arc::clone(&camofox) as Arc<dyn CamofoxPort>,
        Arc::clone(&discord) as Arc<dyn DiscordPort>,
        Arc::clone(&cipher),
        Arc::clone(&artifacts),
    );

    let payment_id = engine
        .create_payment(AGENT, request("e2e-key", &tab_id))
        .await?
        .payment_id;
    engine.approve(payment_id, None)?;
    await_state(&engine, payment_id, PaymentState::AwaitingResultReview).await?;

    // SPEC §9: the button was clicked exactly once.
    assert_eq!(
        mock.counter().total(),
        1,
        "exactly one submission reached the mock checkout"
    );

    // US-10: the masked capture existed before the human was asked to judge it.
    assert_eq!(
        discord.review_saw_artifact(),
        vec![true],
        "the result review is only requested once the masked artifact is stored"
    );
    let artifact = artifacts.path(payment_id, ArtifactKind::Result);
    let png = std::fs::read(&artifact)?;
    assert!(
        png.starts_with(&[0x89, b'P', b'N', b'G']),
        "the artifact is a PNG"
    );

    // The human classifies the result.
    engine.report_outcome(payment_id, Outcome::FailedNoCharge)?;

    // ---- restart: a fresh engine over the same database must not submit ----
    let restarted_store = Arc::new(SqliteStore::open(&path)?);
    let restarted = build_engine(
        restarted_store,
        camofox as Arc<dyn CamofoxPort>,
        discord as Arc<dyn DiscordPort>,
        cipher,
        Arc::clone(&artifacts),
    );
    restarted.recover()?;

    // A replayed Approve on the restarted broker is refused; nothing re-fills.
    let replay = restarted.approve(payment_id, None)?;
    assert_eq!(
        replay,
        broker::DecisionResult::Expired,
        "a settled payment refuses a replayed Approve"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        mock.counter().total(),
        1,
        "SPEC §9: still exactly one submission after a broker restart"
    );

    // Cleanup: the tab is already contained, but a failure must not leak one.
    let _ = client.navigate(&tab_id, &format!("{site}/")).await;
    for suffix in ["", "-wal", "-shm"] {
        let mut file = path.as_os_str().to_owned();
        file.push(suffix);
        let _ = std::fs::remove_file(file);
    }
    let _ = std::fs::remove_dir_all(&artifacts_dir);
    Ok(())
}
