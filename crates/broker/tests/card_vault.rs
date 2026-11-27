//! Integration tests for the long-term card vault and the approval-time card
//! choice (ADR 0002 amended 2026-11-21, ADR 0007).
//!
//! Everything runs in-process against a real SQLite file. The ports are a small
//! driver that records what the broker asked for; it captures only the last
//! four digits of the decrypted card, never the value, and never prints one
//! (AGENTS.md rule 1, US-14).
//!
//! Card-shaped test data is built at runtime (`"4".repeat(16)`), so no source
//! file contains a digit run — that is what `script/no-card-digits` enforces.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use broker::{
    ApprovalRequest, BoxFuture, CamofoxPort, CardCipher, CardId, CardRecord, Cart, CartItem,
    DiscordPort, EngineConfig, FieldKind, FieldSpec, FillRequest, NewPayment, PaymentEngine,
    PaymentEvent, PaymentId, PaymentState, PortError, ResultReviewRequest, ScreenshotStore,
    SqliteStore, StoreError, TransitionEffect,
};

type TestResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

const AGENT: &str = "jamie";

// ---------------------------------------------------------------------------
// Recording driver
// ---------------------------------------------------------------------------

/// One `request_approval` call: the card listing the human saw.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ApprovalSnapshot {
    card_ids: Vec<CardId>,
    pet_names: Vec<String>,
    last4s: Vec<String>,
}

/// One `fill_fields` call: which payment, and the last four of the card the
/// broker decrypted for it. The full card is never stored or printed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FillSnapshot {
    payment_id: PaymentId,
    last4: String,
}

#[derive(Debug, Default)]
struct Driver {
    approvals: Mutex<Vec<ApprovalSnapshot>>,
    fills: Mutex<Vec<FillSnapshot>>,
    clicks: Mutex<Vec<PaymentId>>,
}

impl Driver {
    fn approvals(&self) -> Vec<ApprovalSnapshot> {
        self.approvals
            .lock()
            .map(|entries| entries.clone())
            .unwrap_or_default()
    }

    fn fills(&self) -> Vec<FillSnapshot> {
        self.fills
            .lock()
            .map(|entries| entries.clone())
            .unwrap_or_default()
    }

    fn clicks(&self) -> usize {
        self.clicks.lock().map(|ids| ids.len()).unwrap_or_default()
    }
}

impl CamofoxPort for Driver {
    fn fill_fields<'a>(&'a self, request: FillRequest<'a>) -> BoxFuture<'a, Result<(), PortError>> {
        // Only the last four leave this function: the card itself is dropped
        // with the request.
        let snapshot = FillSnapshot {
            payment_id: request.payment_id,
            last4: request.card.last4(),
        };
        if let Ok(mut fills) = self.fills.lock() {
            fills.push(snapshot);
        }
        Box::pin(async { Ok(()) })
    }

    fn click_submit<'a>(
        &'a self,
        payment_id: PaymentId,
        _tab_id: &'a str,
        _submit_selector: &'a str,
    ) -> BoxFuture<'a, Result<(), PortError>> {
        if let Ok(mut clicks) = self.clicks.lock() {
            clicks.push(payment_id);
        }
        Box::pin(async { Ok(()) })
    }

    fn wait_for_settle<'a>(
        &'a self,
        _payment_id: PaymentId,
        _tab_id: &'a str,
    ) -> BoxFuture<'a, Result<(), PortError>> {
        Box::pin(async { Ok(()) })
    }

    fn capture_masked_result<'a>(
        &'a self,
        _payment_id: PaymentId,
        _tab_id: &'a str,
        _fields: &'a [FieldSpec],
    ) -> BoxFuture<'a, Result<Vec<u8>, PortError>> {
        // One PNG signature; this crate's tests never inspect the pixels.
        Box::pin(async { Ok(vec![0x89, b'P', b'N', b'G']) })
    }

    fn contain_tab<'a>(
        &'a self,
        _payment_id: PaymentId,
        _tab_id: &'a str,
    ) -> BoxFuture<'a, Result<(), PortError>> {
        Box::pin(async { Ok(()) })
    }
}

impl DiscordPort for Driver {
    fn request_approval(
        &self,
        request: ApprovalRequest<'_>,
    ) -> BoxFuture<'_, Result<(), PortError>> {
        let snapshot = ApprovalSnapshot {
            card_ids: request.cards.iter().map(|card| card.card_id).collect(),
            pet_names: request
                .cards
                .iter()
                .map(|card| card.pet_name.clone())
                .collect(),
            last4s: request
                .cards
                .iter()
                .map(|card| card.last4.clone())
                .collect(),
        };
        if let Ok(mut approvals) = self.approvals.lock() {
            approvals.push(snapshot);
        }
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
    store: Arc<SqliteStore>,
    engine: PaymentEngine,
    driver: Arc<Driver>,
    cipher: Arc<CardCipher>,
    artifacts: Arc<ScreenshotStore>,
    artifacts_dir: PathBuf,
    path: PathBuf,
}

impl Harness {
    fn new(tag: &str) -> TestResult<Self> {
        let path = temp_db_path(tag);
        let store = Arc::new(SqliteStore::open(&path)?);
        let driver = Arc::new(Driver::default());
        let cipher = Arc::new(CardCipher::generate());
        let artifacts_dir = temp_artifacts_dir(tag);
        let artifacts = Arc::new(ScreenshotStore::open(&artifacts_dir)?);
        let engine = PaymentEngine::new(
            Arc::clone(&store),
            Arc::clone(&driver) as Arc<dyn CamofoxPort>,
            Arc::clone(&driver) as Arc<dyn DiscordPort>,
            Arc::clone(&cipher),
            Arc::clone(&artifacts),
            EngineConfig::default(),
        );
        Ok(Self {
            store,
            engine,
            driver,
            cipher,
            artifacts,
            artifacts_dir,
            path,
        })
    }

    /// A second engine over the same database, as a restart would build.
    fn restart(&self) -> PaymentEngine {
        PaymentEngine::new(
            Arc::clone(&self.store),
            Arc::clone(&self.driver) as Arc<dyn CamofoxPort>,
            Arc::clone(&self.driver) as Arc<dyn DiscordPort>,
            Arc::clone(&self.cipher),
            Arc::clone(&self.artifacts),
            EngineConfig::default(),
        )
    }

    fn create(&self, key: &str) -> TestResult<PaymentId> {
        Ok(self
            .engine
            .store()
            .insert_pending(AGENT, &request(key), i64::MAX)?)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        remove_db(&self.path);
        let _ = std::fs::remove_dir_all(&self.artifacts_dir);
    }
}

fn temp_artifacts_dir(tag: &str) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "broker-card-vault-artifacts-{tag}-{}-{seq}",
        std::process::id()
    ))
}

fn temp_db_path(tag: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "broker-vault-{tag}-{}-{seq}.db",
        std::process::id()
    ))
}

fn remove_db(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut file = path.as_os_str().to_owned();
        file.push(suffix);
        let _ = std::fs::remove_file(file);
    }
}

fn request(idempotency_key: &str) -> NewPayment {
    NewPayment {
        idempotency_key: idempotency_key.to_string(),
        tab_id: "tab-1".to_string(),
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
        fields: vec![FieldSpec {
            selector: "#card".to_string(),
            field_kind: FieldKind::CardNumber,
        }],
        submit_selector: "#pay".to_string(),
    }
}

/// Synthetic card values, built at runtime so no digit run is committed.
/// `last4` distinguishes otherwise identical test cards.
fn card_record(last4: &str) -> TestResult<CardRecord> {
    Ok(CardRecord::parse(
        &format!("{}{last4}", "7".repeat(12)),
        "12/34",
        &"1".repeat(3),
        "Test Holder",
        &"1".repeat(5),
    )?)
}

/// Every byte of the database (main file plus WAL), for the "ciphertext only"
/// assertion.
fn database_bytes(path: &Path) -> TestResult<Vec<u8>> {
    let mut bytes = std::fs::read(path)?;
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = path.as_os_str().to_owned();
        sidecar.push(suffix);
        if let Ok(mut extra) = std::fs::read(PathBuf::from(sidecar)) {
            bytes.append(&mut extra);
        }
    }
    Ok(bytes)
}

// ---------------------------------------------------------------------------
// The vault
// ---------------------------------------------------------------------------

/// The whole persisted form of a card is: pet name, last four, and an age
/// blob (ADR 0002 — "ciphertext is the only persisted form").
#[tokio::test]
async fn add_card_stores_only_ciphertext_in_the_database_file() -> TestResult<()> {
    let harness = Harness::new("ciphertext-only")?;
    let card = card_record("4444")?;
    let pan = card.fields().card_number.to_string();

    let summary = harness
        .store
        .add_card(&harness.cipher, "Groceries", &card)?;

    let viewer = String::from_utf8_lossy(&database_bytes(&harness.path)?).into_owned();
    assert!(
        !viewer.contains(&pan),
        "the PAN must never reach the database file in the clear"
    );
    assert!(
        viewer.contains("Groceries"),
        "the pet name is display data and is stored in clear"
    );
    assert_eq!(summary.last4, pan[pan.len() - 4..].to_string());

    let blob = harness
        .store
        .card_blob(summary.card_id)?
        .ok_or("the blob must be stored")?;
    let armored = String::from_utf8(blob)?;
    assert!(
        armored.starts_with("-----BEGIN AGE ENCRYPTED FILE-----"),
        "the stored form is an armored age file"
    );
    assert!(!armored.contains(&pan), "the blob is ciphertext");
    Ok(())
}

/// The listing the approval flow renders: id, pet name, last four. Nothing
/// else exists to leak.
#[tokio::test]
async fn card_listing_returns_pet_name_and_last4_only() -> TestResult<()> {
    let harness = Harness::new("listing")?;
    harness
        .store
        .add_card(&harness.cipher, "Groceries", &card_record("4444")?)?;
    harness
        .store
        .add_card(&harness.cipher, "Travel", &card_record("5555")?)?;

    let cards = harness.store.cards()?;

    assert_eq!(cards.len(), 2);
    assert_eq!(
        cards
            .iter()
            .map(|card| card.pet_name.as_str())
            .collect::<Vec<_>>(),
        vec!["Groceries", "Travel"],
        "the listing is ordered by pet name"
    );
    for card in &cards {
        assert_eq!(card.last4.chars().count(), 4, "only four digits are stored");
    }
    Ok(())
}

#[tokio::test]
async fn duplicate_pet_names_are_rejected() -> TestResult<()> {
    let harness = Harness::new("duplicate-name")?;
    harness
        .store
        .add_card(&harness.cipher, "Groceries", &card_record("4444")?)?;

    let second = harness
        .store
        .add_card(&harness.cipher, "Groceries", &card_record("5555")?);

    assert!(matches!(
        second,
        Err(StoreError::DuplicateCardName(name)) if name == "Groceries"
    ));
    Ok(())
}

/// The approval message carries the card listing — pet name and last four per
/// card — so the human can confirm or choose (requirement 2026-11-21).
#[tokio::test]
async fn approval_request_carries_card_options_pet_name_and_last4() -> TestResult<()> {
    let harness = Harness::new("approval-options")?;
    let groceries = harness
        .store
        .add_card(&harness.cipher, "Groceries", &card_record("4444")?)?;
    let travel = harness
        .store
        .add_card(&harness.cipher, "Travel", &card_record("5555")?)?;

    harness
        .engine
        .create_payment(AGENT, request("options-key"))
        .await?;

    let approvals = harness.driver.approvals();
    assert_eq!(approvals.len(), 1, "the human is asked once");
    let listing = &approvals[0];
    assert_eq!(listing.card_ids, vec![groceries.card_id, travel.card_id]);
    assert_eq!(listing.pet_names, vec!["Groceries", "Travel"]);
    assert_eq!(
        listing.last4s,
        vec![groceries.last4.clone(), travel.last4.clone()]
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Card choice at approval
// ---------------------------------------------------------------------------

#[tokio::test]
async fn approve_auto_selects_the_only_registered_card() -> TestResult<()> {
    let harness = Harness::new("auto-select")?;
    let only = harness
        .store
        .add_card(&harness.cipher, "Groceries", &card_record("4444")?)?;
    let payment_id = harness.create("auto-select-key")?;

    let decision = harness.engine.approve(payment_id, None)?;

    assert_eq!(decision, broker::DecisionResult::Applied);
    assert_eq!(
        harness.store.load(payment_id)?.chosen_card,
        Some(only.card_id),
        "the single card is selected for the human"
    );
    Ok(())
}

/// The card and the approval are one commit: there is no instant at which a
/// payment is `approved` without the card it will pay with (ADR 0007).
#[tokio::test]
async fn approve_binds_the_chosen_card_in_the_same_commit_as_approval() -> TestResult<()> {
    let harness = Harness::new("same-commit")?;
    let first = harness
        .store
        .add_card(&harness.cipher, "Groceries", &card_record("4444")?)?;
    let payment_id = harness.create("same-commit-key")?;

    // Exercised at the store level: the engine spawns the fill pipeline the
    // instant it commits, so only a direct transition can observe the row
    // exactly as committed.
    let approved = harness.store.transition(
        payment_id,
        PaymentState::PendingApproval,
        PaymentEvent::Approve,
        &TransitionEffect {
            chosen_card: Some(first.card_id),
            ..TransitionEffect::default()
        },
    )?;

    assert_eq!(approved.state, PaymentState::Approved);
    assert_eq!(approved.chosen_card, Some(first.card_id));
    let history = harness.store.history(payment_id)?;
    assert_eq!(
        history.last().map(|(_, to, _)| *to),
        Some(PaymentState::Approved),
        "the approval is one history entry"
    );
    Ok(())
}

#[tokio::test]
async fn approve_without_a_choice_and_multiple_cards_is_refused() -> TestResult<()> {
    let harness = Harness::new("no-choice")?;
    harness
        .store
        .add_card(&harness.cipher, "Groceries", &card_record("4444")?)?;
    harness
        .store
        .add_card(&harness.cipher, "Travel", &card_record("5555")?)?;
    let payment_id = harness.create("no-choice-key")?;

    let decision = harness.engine.approve(payment_id, None)?;
    let stored = harness.store.load(payment_id)?;

    assert_eq!(decision, broker::DecisionResult::Refused);
    assert_eq!(stored.state, PaymentState::PendingApproval, "nothing moved");
    assert_eq!(stored.chosen_card, None, "no card is guessed");
    assert!(harness.driver.fills().is_empty(), "nothing was filled");
    assert_eq!(harness.driver.clicks(), 0, "nothing was submitted");
    Ok(())
}

#[tokio::test]
async fn approve_with_unknown_card_id_is_refused() -> TestResult<()> {
    let harness = Harness::new("unknown-card")?;
    harness
        .store
        .add_card(&harness.cipher, "Groceries", &card_record("4444")?)?;
    let payment_id = harness.create("unknown-card-key")?;

    let decision = harness.engine.approve(payment_id, Some(CardId::new()))?;

    assert_eq!(decision, broker::DecisionResult::Refused);
    assert_eq!(
        harness.store.load(payment_id)?.chosen_card,
        None,
        "a card that does not exist is never recorded"
    );
    assert_eq!(harness.driver.clicks(), 0);
    Ok(())
}

#[tokio::test]
async fn approve_with_zero_cards_is_refused() -> TestResult<()> {
    let harness = Harness::new("zero-cards")?;
    let payment_id = harness.create("zero-cards-key")?;

    let decision = harness.engine.approve(payment_id, None)?;

    assert_eq!(decision, broker::DecisionResult::Refused);
    assert_eq!(
        harness.store.load(payment_id)?.state,
        PaymentState::PendingApproval
    );
    assert!(harness.driver.fills().is_empty());
    Ok(())
}

// ---------------------------------------------------------------------------
// Fill path
// ---------------------------------------------------------------------------

/// The fill call carries the values of the card the human chose — and only
/// that card (AGENTS.md rule 7).
#[tokio::test]
async fn fill_passes_the_chosen_cards_decrypted_values_to_the_camofox_port() -> TestResult<()> {
    let harness = Harness::new("fill-chosen")?;
    let groceries = harness
        .store
        .add_card(&harness.cipher, "Groceries", &card_record("4444")?)?;
    let travel = harness
        .store
        .add_card(&harness.cipher, "Travel", &card_record("5555")?)?;
    let payment_id = harness.create("fill-chosen-key")?;

    assert_eq!(
        harness.engine.approve(payment_id, Some(travel.card_id))?,
        broker::DecisionResult::Applied
    );
    await_state(
        &harness.engine,
        payment_id,
        PaymentState::AwaitingResultReview,
    )
    .await?;

    let fills = harness.driver.fills();
    assert_eq!(fills.len(), 1, "exactly one fill per payment");
    assert_eq!(fills[0].payment_id, payment_id);
    assert_eq!(
        fills[0].last4, travel.last4,
        "the values typed are the chosen card's, not another card's"
    );
    assert_ne!(
        fills[0].last4, groceries.last4,
        "the other card's values never reach the port"
    );
    Ok(())
}

/// A blob this deployment cannot open aborts before submit — nothing is
/// clicked, so nothing can be charged.
#[tokio::test]
async fn undecryptable_blob_aborts_before_submit() -> TestResult<()> {
    let harness = Harness::new("corrupt-blob")?;
    let card = harness
        .store
        .add_card(&harness.cipher, "Groceries", &card_record("4444")?)?;
    let payment_id = harness.create("corrupt-blob-key")?;

    // Corrupt the ciphertext between approval and fill, as a damaged volume
    // would.
    let connection = rusqlite::Connection::open(&harness.path)?;
    connection.execute(
        "UPDATE cards SET blob = X'00' WHERE card_id = ?1",
        rusqlite::params![card.card_id.to_string()],
    )?;
    drop(connection);

    harness.engine.approve(payment_id, None)?;
    let view = await_state(
        &harness.engine,
        payment_id,
        PaymentState::AbortedBeforeSubmit,
    )
    .await?;

    assert_eq!(view.state, PaymentState::AbortedBeforeSubmit);
    assert!(!harness.store.load(payment_id)?.submit_attempted);
    assert_eq!(
        harness.driver.clicks(),
        0,
        "an unreadable card must never be submitted"
    );
    Ok(())
}

/// A card that a live payment may still fill with cannot be retired — that
/// payment has not settled, so the card must stay resolvable (ADR 0007).
#[tokio::test]
async fn retiring_a_card_in_use_by_a_live_payment_is_refused() -> TestResult<()> {
    let harness = Harness::new("retire-live")?;
    let card = harness
        .store
        .add_card(&harness.cipher, "Groceries", &card_record("4444")?)?;
    let payment_id = harness.create("retire-live-key")?;
    harness.engine.approve(payment_id, None)?;
    await_state(
        &harness.engine,
        payment_id,
        PaymentState::AwaitingResultReview,
    )
    .await?;

    let refused = harness.store.retire_card(card.card_id);

    assert!(matches!(refused, Err(StoreError::CardInUse(id)) if id == card.card_id));
    assert_eq!(
        harness.store.cards()?.len(),
        1,
        "a refused retirement changes nothing"
    );
    Ok(())
}

/// Retiring keeps the row (so payment history keeps its foreign key) but takes
/// the card out of the approval listing and out of the choice resolution.
#[tokio::test]
async fn retiring_a_card_hides_it_but_keeps_payment_history() -> TestResult<()> {
    let harness = Harness::new("retire")?;
    let card = harness
        .store
        .add_card(&harness.cipher, "Groceries", &card_record("4444")?)?;
    harness
        .store
        .add_card(&harness.cipher, "Travel", &card_record("5555")?)?;
    let payment_id = harness.create("retire-key")?;
    // Two cards are registered, so the click must name the one to pay with.
    harness.engine.approve(payment_id, Some(card.card_id))?;
    await_state(
        &harness.engine,
        payment_id,
        PaymentState::AwaitingResultReview,
    )
    .await?;
    // Settle it, so the card is no longer in use.
    harness
        .engine
        .report_outcome(payment_id, broker::Outcome::FailedNoCharge)?;

    assert!(harness.store.retire_card(card.card_id)?);

    assert_eq!(
        harness.store.cards()?.len(),
        1,
        "a retired card is not offered"
    );
    assert!(
        harness.store.active_card(card.card_id)?.is_none(),
        "a retired card cannot be chosen"
    );
    assert_eq!(
        harness.store.load(payment_id)?.chosen_card,
        Some(card.card_id),
        "the settled payment keeps the card it paid with"
    );
    assert!(
        harness.store.card_blob(card.card_id)?.is_some(),
        "history stays readable; only the listing hides it"
    );
    Ok(())
}

/// A retired card can never be chosen, and retiring frees its pet name.
#[tokio::test]
async fn a_retired_card_cannot_be_chosen_and_its_name_is_reusable() -> TestResult<()> {
    let harness = Harness::new("retire-reuse")?;
    let retired = harness
        .store
        .add_card(&harness.cipher, "Groceries", &card_record("4444")?)?;
    assert!(harness.store.retire_card(retired.card_id)?);

    // The partial unique index lets the name come back.
    let reused = harness
        .store
        .add_card(&harness.cipher, "Groceries", &card_record("5555")?)?;
    let payment_id = harness.create("retire-reuse-key")?;

    assert_eq!(
        harness.engine.approve(payment_id, Some(retired.card_id))?,
        broker::DecisionResult::Refused,
        "a retired card is not a usable choice"
    );
    assert_eq!(
        harness.engine.approve(payment_id, Some(reused.card_id))?,
        broker::DecisionResult::Applied
    );
    Ok(())
}

/// A failed card-listing read happens *before* anything is persisted: the POST
/// fails and no half-created payment (no armed hold timer) is left behind
/// (US-6: a gateway-side failure must never strand a payment).
#[tokio::test]
async fn create_payment_fails_cleanly_when_the_listing_is_unreadable() -> TestResult<()> {
    let harness = Harness::new("listing-failure")?;
    let connection = rusqlite::Connection::open(&harness.path)?;
    connection.execute("DROP TABLE cards", [])?;
    drop(connection);

    let outcome = harness
        .engine
        .create_payment(AGENT, request("listing-failure-key"))
        .await;

    assert!(outcome.is_err(), "the POST reports the failure");
    assert_eq!(
        harness
            .store
            .find_by_idempotency_key(AGENT, "listing-failure-key")?,
        None,
        "no payment row exists without its approval message and hold timer"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Durability
// ---------------------------------------------------------------------------

/// Both the card choice and the vault survive a restart; crash recovery keeps
/// its meaning (US-12).
#[tokio::test]
async fn chosen_card_survives_restart_and_recovery_semantics_hold() -> TestResult<()> {
    let harness = Harness::new("restart-choice")?;
    let card = harness
        .store
        .add_card(&harness.cipher, "Groceries", &card_record("4444")?)?;
    let payment_id = harness.create("restart-choice-key")?;
    harness.engine.approve(payment_id, None)?;

    let restarted = harness.restart();

    assert_eq!(
        restarted.store().load(payment_id)?.chosen_card,
        Some(card.card_id),
        "the choice is on disk, not in memory"
    );

    // A `filling` payment found at boot aborts before submit, whatever card it
    // had chosen.
    let filling = harness.create("restart-filling-key")?;
    for (from, event, effect) in [
        (
            PaymentState::PendingApproval,
            PaymentEvent::Approve,
            TransitionEffect {
                chosen_card: Some(card.card_id),
                ..TransitionEffect::default()
            },
        ),
        (
            PaymentState::Approved,
            PaymentEvent::FillStarted,
            TransitionEffect::default(),
        ),
    ] {
        harness.store.transition(filling, from, event, &effect)?;
    }
    restarted.recover()?;

    assert_eq!(
        restarted.store().load(filling)?.state,
        PaymentState::AbortedBeforeSubmit
    );
    assert_eq!(
        restarted.store().load(filling)?.chosen_card,
        Some(card.card_id),
        "recovery does not lose the record of what was chosen"
    );
    Ok(())
}

/// The vault outlives the process: the blob written before the restart still
/// decrypts afterwards with the same identity (requirement 2026-11-21: cards
/// are long-term, so the key must be too).
#[tokio::test]
async fn card_blob_survives_restart() -> TestResult<()> {
    let harness = Harness::new("vault-restart")?;
    let card = card_record("4444")?;
    let pan = card.fields().card_number.to_string();
    let summary = harness
        .store
        .add_card(&harness.cipher, "Groceries", &card)?;

    let reopened = SqliteStore::open(&harness.path)?;
    let blob = reopened
        .card_blob(summary.card_id)?
        .ok_or("blob is durable")?;
    let decrypted = harness.cipher.decrypt(&blob)?;

    assert_eq!(decrypted.fields().card_number, pan);
    Ok(())
}

async fn await_state(
    engine: &PaymentEngine,
    payment_id: PaymentId,
    want: PaymentState,
) -> TestResult<broker::PaymentView> {
    for _ in 0..500 {
        let view = engine.view(payment_id)?;
        if view.state == want {
            return Ok(view);
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let actual = engine.view(payment_id)?.state;
    Err(format!("expected {want:?}, still {actual:?}").into())
}
