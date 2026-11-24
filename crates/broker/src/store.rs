//! Durable payment state in SQLite (ADR 0001).
//!
//! Why the database and not a map: the broker's obligations here are
//! *durability* obligations, not request-scoped ones — the submit-once latch
//! must survive a crash (spec §7, US-12), idempotency must dedupe across
//! restarts (US-13), and a payment found in flight at boot must be recoverable
//! without any in-memory history (US-12).
//!
//! Schema notes (ADR 0001, amended 2026-11-21):
//!
//! * `payments` — one row per payment. `(agent_id, idempotency_key)` carries a
//!   `UNIQUE` index, so dedupe is enforced by SQLite rather than by a
//!   read-then-write race in application code. `chosen_card` records which
//!   vault card the human picked at the approval click, set in the same
//!   commit as `pending_approval → approved`.
//! * `transitions` — append-only history `(payment_id, from_state, to_state,
//!   at, reason)`. The state machine is the product, so its history is
//!   queryable with `sqlite3` during an incident.
//! * `submit_attempted` / `submitted_at` — the submit-once latch, written and
//!   committed *before* the click is issued (AGENTS.md rule 3).
//! * `cards` — the long-term vault (ADR 0002, amended 2026-11-21): pet name
//!   and last four in clear (PCI-permissible display data), and the full card
//!   as an `age` ciphertext blob. No column here holds a PAN, expiry or CVV.
//!   The blob is written by the registration CLI and decrypted only by the
//!   engine's approved fill path (`crate::crypto`).
//!
//! Concurrency: one write connection behind a mutex (writes are serialized,
//! transitions are short) and a second connection for reads, so a long-poll
//! waiter never holds the write connection (ADR 0001, "Bad / accepted costs").
//! No lock is held across an `await`; every helper returns owned values.

use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};

use crate::crypto::CardCipher;
use crate::payment::{
    CardId, CardSummary, Cart, FieldSpec, NewPayment, OrderReference, PaymentId, PaymentView,
    TerminalReason,
};
use crate::state::PaymentState;

/// Current schema version (`PRAGMA user_version`).
const SCHEMA_VERSION: i64 = 2;

/// Longest accepted pet name.
pub const MAX_PET_NAME_LEN: usize = 48;

/// One persisted payment, as read back from SQLite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredPayment {
    /// The payment id.
    pub payment_id: PaymentId,
    /// The agent that created it (from its API key).
    pub agent_id: String,
    /// The agent's dedupe key.
    pub idempotency_key: String,
    /// Current state.
    pub state: PaymentState,
    /// The agent-reported cart.
    pub cart: Cart,
    /// The Camofox tab bound to this payment.
    pub tab_id: String,
    /// Where each card value would go.
    pub fields: Vec<FieldSpec>,
    /// The submit button selector.
    pub submit_selector: String,
    /// Set by a `succeeded` result review.
    pub order_reference: Option<OrderReference>,
    /// Why the payment is terminal.
    pub terminal_reason: Option<TerminalReason>,
    /// Whether the submit latch has been committed (spec §7).
    pub submit_attempted: bool,
    /// When the latch was committed.
    pub submitted_at: Option<i64>,
    /// The vault card the human chose at the approval click (ADR 0007).
    pub chosen_card: Option<CardId>,
    /// Unix seconds at which a pending approval expires.
    pub approval_deadline: i64,
    /// Unix seconds the payment was created.
    pub created_at: i64,
    /// Unix seconds of the last transition.
    pub updated_at: i64,
}

impl StoredPayment {
    /// Project the row onto the only shape the HTTP API may return.
    ///
    /// The projection is explicit rather than a `#[serde(flatten)]` so that
    /// adding a column can never silently widen the API response (AGENTS.md
    /// rule 6).
    #[must_use]
    pub fn view(&self) -> PaymentView {
        PaymentView {
            payment_id: self.payment_id,
            state: self.state,
            cart: self.cart.clone(),
            fields: self.fields.clone(),
            submit_selector: self.submit_selector.clone(),
            tab_id: self.tab_id.clone(),
            approval_deadline: self.approval_deadline,
            order_reference: self.order_reference.clone(),
            terminal_reason: self.terminal_reason,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

/// Extra columns a transition writes in the same commit as the state change.
#[derive(Debug, Clone, Default)]
pub struct TransitionEffect {
    /// Terminal reason to record (never overwrites an existing one).
    pub terminal_reason: Option<TerminalReason>,
    /// Order reference to record.
    pub order_reference: Option<OrderReference>,
    /// Commit the submit-once latch with this transition (spec §7).
    pub submit_attempted: bool,
    /// The human's card choice, committed with the approval transition.
    pub chosen_card: Option<CardId>,
}

/// Anything that can go wrong in the store. Carries ids and states only — no
/// card data can reach an error message from here (AGENTS.md rule 1).
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// SQLite failed.
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The database contains JSON we cannot read back.
    #[error("could not decode stored payment: {0}")]
    Decode(#[from] serde_json::Error),
    /// No such payment.
    #[error("no such payment: {0}")]
    NotFound(PaymentId),
    /// The row is already in a different state than the caller assumed.
    #[error("payment {payment_id} is in state {current}, expected {expected}")]
    Conflict {
        /// The payment.
        payment_id: PaymentId,
        /// The state the caller transitioned from.
        expected: PaymentState,
        /// The state actually stored.
        current: PaymentState,
    },
    /// The state machine refuses this (state, event) pair (spec §4).
    #[error(transparent)]
    Illegal(crate::state::IllegalTransition),
    /// The idempotency key is already used by this agent.
    #[error("duplicate idempotency key for payment {0}")]
    Duplicate(PaymentId),
    /// No such card.
    #[error("no such card: {0}")]
    CardNotFound(CardId),
    /// A card with this pet name already exists.
    #[error("a card named {0:?} already exists")]
    DuplicateCardName(String),
    /// The pet name is unusable; the string names the rule only.
    #[error("invalid card name: {0}")]
    InvalidCardName(&'static str),
    /// A stored row is unreadable.
    #[error("stored payment is corrupt: {0}")]
    Corrupt(&'static str),
    /// The database mutex was poisoned by a panicking writer.
    #[error("payment store is unavailable")]
    Unavailable,
    /// Card encryption failed while writing the vault.
    #[error("card encryption failed")]
    Crypto,
    /// A schema this build does not understand.
    #[error("unsupported schema version {0}")]
    UnsupportedSchema(i64),
}

/// The SQLite-backed payment store.
#[derive(Debug)]
pub struct SqliteStore {
    writer: Mutex<Connection>,
    reader: Mutex<Connection>,
}

impl SqliteStore {
    /// Open (and migrate) the database at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the file cannot be opened, is corrupt, or
    /// carries a future schema version.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        let writer = Connection::open(&path)?;
        writer.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=FULL;
             PRAGMA foreign_keys=ON;
             PRAGMA busy_timeout=5000;",
        )?;
        migrate(&writer)?;

        // Reads go through their own connection so a poll never queues behind
        // a state-changing write (ADR 0001).
        let reader = Connection::open(&path)?;
        reader.execute_batch("PRAGMA query_only=ON; PRAGMA busy_timeout=5000;")?;

        Ok(Self {
            writer: Mutex::new(writer),
            reader: Mutex::new(reader),
        })
    }

    /// Claim the idempotency key and insert the payment as `pending_approval`.
    ///
    /// # Errors
    ///
    /// [`StoreError::Duplicate`] carries the id of the payment that already
    /// owns the key (US-13), so the caller can answer with the original id.
    pub fn insert_pending(
        &self,
        agent_id: &str,
        request: &NewPayment,
        approval_deadline: i64,
    ) -> Result<PaymentId, StoreError> {
        let now = now_unix();
        let payment_id = PaymentId::new();
        let cart = serde_json::to_string(&request.cart)?;
        let fields = serde_json::to_string(&request.fields)?;

        let mut connection = self.writer.lock().map_err(|_| StoreError::Unavailable)?;
        let transaction = connection.transaction()?;

        let inserted = transaction.execute(
            "INSERT INTO payments (
                 payment_id, agent_id, idempotency_key, state, cart_json, tab_id,
                 fields_json, submit_selector, approval_deadline, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
            params![
                payment_id.to_string(),
                agent_id,
                request.idempotency_key,
                PaymentState::PendingApproval.as_str(),
                cart,
                request.tab_id,
                fields,
                request.submit_selector,
                approval_deadline,
                now,
            ],
        );

        match inserted {
            Ok(_) => {
                transaction.execute(
                    "INSERT INTO transitions (payment_id, from_state, to_state, at, reason)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        payment_id.to_string(),
                        "new",
                        PaymentState::PendingApproval.as_str(),
                        now,
                        "created",
                    ],
                )?;
                transaction.commit()?;
                Ok(payment_id)
            }
            Err(error) if is_unique_violation(&error) => {
                let existing: Option<String> = transaction
                    .query_row(
                        "SELECT payment_id FROM payments WHERE agent_id = ?1 AND idempotency_key = ?2",
                        params![agent_id, request.idempotency_key],
                        |row| row.get(0),
                    )
                    .optional()?;
                // The transaction is dropped (rolled back) — nothing was written.
                drop(transaction);
                match existing.and_then(|id| id.parse().ok()) {
                    Some(id) => Err(StoreError::Duplicate(id)),
                    // The unique index fired but the row is gone: surface the
                    // constraint failure rather than inventing an id.
                    None => Err(StoreError::Sqlite(error)),
                }
            }
            Err(error) => Err(StoreError::Sqlite(error)),
        }
    }

    /// Look up the payment that already owns `idempotency_key` (US-13).
    pub fn find_by_idempotency_key(
        &self,
        agent_id: &str,
        idempotency_key: &str,
    ) -> Result<Option<PaymentId>, StoreError> {
        let connection = self.reader.lock().map_err(|_| StoreError::Unavailable)?;
        let found: Option<String> = connection
            .query_row(
                "SELECT payment_id FROM payments WHERE agent_id = ?1 AND idempotency_key = ?2",
                params![agent_id, idempotency_key],
                |row| row.get(0),
            )
            .optional()?;
        Ok(found.and_then(|id| id.parse().ok()))
    }

    /// Read one payment.
    pub fn load(&self, payment_id: PaymentId) -> Result<StoredPayment, StoreError> {
        let connection = self.reader.lock().map_err(|_| StoreError::Unavailable)?;
        let row = connection
            .query_row(
                &format!("SELECT {SELECT_COLUMNS} FROM payments WHERE payment_id = ?1"),
                params![payment_id.to_string()],
                decode_row,
            )
            .optional()?;
        match row {
            Some(payment) => payment,
            None => Err(StoreError::NotFound(payment_id)),
        }
    }

    /// Apply one transition as a compare-and-set, appending history in the same
    /// commit.
    ///
    /// This is the *only* writer of `payments.state`, and the target state is
    /// not a parameter: it is computed by [`crate::state::transition`]. So the
    /// spec §4 table is the single authority for legal moves — a caller cannot
    /// hand in an arbitrary from→to pair, and an illegal pair (say
    /// `pending_approval → filling`, which would fill a card before approval)
    /// is refused as [`StoreError::Illegal`] before anything is written.
    ///
    /// The `WHERE state = expected` clause additionally makes the transition
    /// authoritative under concurrency: a hold-expiry timer racing an Approve
    /// click produces exactly one winner, and the loser gets
    /// [`StoreError::Conflict`].
    pub fn transition(
        &self,
        payment_id: PaymentId,
        expected: PaymentState,
        event: crate::state::PaymentEvent,
        effect: &TransitionEffect,
    ) -> Result<StoredPayment, StoreError> {
        let next = crate::state::transition(expected, event).map_err(StoreError::Illegal)?;
        let now = now_unix();
        let mut connection = self.writer.lock().map_err(|_| StoreError::Unavailable)?;
        let transaction = connection.transaction()?;

        let changed = transaction.execute(
            "UPDATE payments SET
                 state = ?3,
                 updated_at = ?4,
                 terminal_reason = COALESCE(?5, terminal_reason),
                 order_reference = COALESCE(?6, order_reference),
                 submit_attempted = MAX(submit_attempted, ?7),
                 submitted_at = COALESCE(submitted_at, ?8),
                 chosen_card = COALESCE(?9, chosen_card)
             WHERE payment_id = ?1 AND state = ?2",
            params![
                payment_id.to_string(),
                expected.as_str(),
                next.as_str(),
                now,
                effect.terminal_reason.map(TerminalReason::as_str),
                effect.order_reference.as_ref().map(OrderReference::as_str),
                i64::from(effect.submit_attempted),
                effect.submit_attempted.then_some(now),
                effect.chosen_card.map(|card_id| card_id.to_string()),
            ],
        )?;

        if changed == 0 {
            let current: Option<String> = transaction
                .query_row(
                    "SELECT state FROM payments WHERE payment_id = ?1",
                    params![payment_id.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            drop(transaction);
            return match current.and_then(|state| PaymentState::parse(&state)) {
                Some(current) => Err(StoreError::Conflict {
                    payment_id,
                    expected,
                    current,
                }),
                None => Err(StoreError::NotFound(payment_id)),
            };
        }

        transaction.execute(
            "INSERT INTO transitions (payment_id, from_state, to_state, at, reason)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                payment_id.to_string(),
                expected.as_str(),
                next.as_str(),
                now,
                effect.terminal_reason.map(TerminalReason::as_str),
            ],
        )?;
        transaction.commit()?;

        drop(connection);
        self.load(payment_id)
    }

    /// Every non-terminal payment, for startup recovery (US-12).
    pub fn in_flight(&self) -> Result<Vec<StoredPayment>, StoreError> {
        let connection = self.reader.lock().map_err(|_| StoreError::Unavailable)?;
        let mut statement = connection.prepare(&format!(
            "SELECT {SELECT_COLUMNS} FROM payments WHERE state IN (?1, ?2, ?3, ?4, ?5)"
        ))?;
        let rows = statement.query_map(
            params![
                PaymentState::PendingApproval.as_str(),
                PaymentState::Approved.as_str(),
                PaymentState::Filling.as_str(),
                PaymentState::Submitting.as_str(),
                PaymentState::AwaitingResultReview.as_str(),
            ],
            decode_row,
        )?;

        let mut payments = Vec::new();
        for row in rows {
            payments.push(row??);
        }
        Ok(payments)
    }

    /// Register one card in the long-term vault (ADR 0002, ADR 0007).
    ///
    /// The full card is encrypted before it reaches SQLite; only the pet name
    /// and the last four digits are stored in clear. One `docker exec` per
    /// card — this is the only write path into `cards`.
    ///
    /// # Errors
    ///
    /// [`StoreError::InvalidCardName`], [`StoreError::DuplicateCardName`],
    /// [`StoreError::Crypto`], [`StoreError::Unavailable`].
    pub fn add_card(
        &self,
        cipher: &CardCipher,
        pet_name: &str,
        card: &crate::crypto::CardRecord,
    ) -> Result<CardSummary, StoreError> {
        let pet_name = pet_name.trim();
        if pet_name.is_empty() {
            return Err(StoreError::InvalidCardName("must not be empty"));
        }
        if pet_name.len() > MAX_PET_NAME_LEN {
            return Err(StoreError::InvalidCardName("must be at most 48 bytes"));
        }

        let card_id = CardId::new();
        let last4 = card.last4();
        let blob = cipher.encrypt(card).map_err(|_| StoreError::Crypto)?;
        let now = now_unix();

        let mut connection = self.writer.lock().map_err(|_| StoreError::Unavailable)?;
        let transaction = connection.transaction()?;
        let inserted = transaction.execute(
            "INSERT INTO cards (card_id, pet_name, last4, blob, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            params![card_id.to_string(), pet_name, last4, blob, now],
        );

        match inserted {
            Ok(_) => {
                transaction.commit()?;
                Ok(CardSummary {
                    card_id,
                    pet_name: pet_name.to_string(),
                    last4,
                })
            }
            Err(error) if is_unique_violation(&error) => {
                Err(StoreError::DuplicateCardName(pet_name.to_string()))
            }
            Err(error) => Err(StoreError::Sqlite(error)),
        }
    }

    /// The card-listing surface the approval flow renders: id, pet name,
    /// last four. Never a card value (ADR 0007, spec §6).
    ///
    /// # Errors
    ///
    /// [`StoreError::Sqlite`], [`StoreError::Unavailable`].
    pub fn cards(&self) -> Result<Vec<CardSummary>, StoreError> {
        let connection = self.reader.lock().map_err(|_| StoreError::Unavailable)?;
        let mut statement = connection.prepare(
            "SELECT card_id, pet_name, last4 FROM cards ORDER BY pet_name ASC, created_at ASC",
        )?;
        let rows = statement.query_map([], |row| {
            let card_id: String = row.get(0)?;
            let pet_name: String = row.get(1)?;
            let last4: String = row.get(2)?;
            Ok((card_id, pet_name, last4))
        })?;

        let mut cards = Vec::new();
        for row in rows {
            let (card_id, pet_name, last4) = row?;
            let card_id = card_id
                .parse()
                .map_err(|_| StoreError::Corrupt("malformed card id"))?;
            cards.push(CardSummary {
                card_id,
                pet_name,
                last4,
            });
        }
        Ok(cards)
    }

    /// One card's ciphertext, or `None` when it is not registered.
    ///
    /// Ciphertext only: reading it is harmless by construction, decrypting it
    /// is [`crate::crypto::CardCipher::decrypt`]'s job inside the approved
    /// fill path.
    ///
    /// # Errors
    ///
    /// [`StoreError::Sqlite`], [`StoreError::Unavailable`].
    pub fn card_blob(&self, card_id: CardId) -> Result<Option<Vec<u8>>, StoreError> {
        let connection = self.reader.lock().map_err(|_| StoreError::Unavailable)?;
        let blob: Option<Vec<u8>> = connection
            .query_row(
                "SELECT blob FROM cards WHERE card_id = ?1",
                params![card_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(blob)
    }

    /// Forget one card. Returns whether a row was removed.
    ///
    /// # Errors
    ///
    /// [`StoreError::Sqlite`], [`StoreError::Unavailable`].
    pub fn remove_card(&self, card_id: CardId) -> Result<bool, StoreError> {
        let connection = self.writer.lock().map_err(|_| StoreError::Unavailable)?;
        let removed = connection.execute(
            "DELETE FROM cards WHERE card_id = ?1",
            params![card_id.to_string()],
        )?;
        Ok(removed > 0)
    }

    /// The append-only transition history for one payment, as
    /// `(from, to, reason)` triples — the debugging view ADR 0001 promises.
    pub fn history(
        &self,
        payment_id: PaymentId,
    ) -> Result<Vec<(String, PaymentState, Option<String>)>, StoreError> {
        let connection = self.reader.lock().map_err(|_| StoreError::Unavailable)?;
        let mut statement = connection.prepare(
            "SELECT from_state, to_state, reason FROM transitions
             WHERE payment_id = ?1 ORDER BY seq ASC",
        )?;
        let rows = statement.query_map(params![payment_id.to_string()], |row| {
            let from: String = row.get(0)?;
            let to: String = row.get(1)?;
            let reason: Option<String> = row.get(2)?;
            Ok((from, to, reason))
        })?;

        let mut history = Vec::new();
        for row in rows {
            let (from, to, reason) = row?;
            let to = PaymentState::parse(&to).ok_or(StoreError::Corrupt("unknown state"))?;
            history.push((from, to, reason));
        }
        Ok(history)
    }
}

const SELECT_COLUMNS: &str = "payment_id, agent_id, idempotency_key, state, cart_json, tab_id, \
     fields_json, submit_selector, terminal_reason, order_reference, submit_attempted, \
     submitted_at, chosen_card, approval_deadline, created_at, updated_at";

fn migrate(connection: &Connection) -> Result<(), StoreError> {
    let mut version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version == SCHEMA_VERSION {
        return Ok(());
    }
    if !(0..=SCHEMA_VERSION).contains(&version) {
        return Err(StoreError::UnsupportedSchema(version));
    }

    if version == 0 {
        connection.execute_batch(
            "BEGIN;
             CREATE TABLE payments (
                 payment_id        TEXT PRIMARY KEY,
                 agent_id          TEXT NOT NULL,
                 idempotency_key   TEXT NOT NULL,
                 state             TEXT NOT NULL,
                 cart_json         TEXT NOT NULL,
                 tab_id            TEXT NOT NULL,
                 fields_json       TEXT NOT NULL,
                 submit_selector   TEXT NOT NULL,
                 terminal_reason   TEXT,
                 order_reference   TEXT,
                 submit_attempted  INTEGER NOT NULL DEFAULT 0,
                 submitted_at      INTEGER,
                 approval_deadline INTEGER NOT NULL,
                 created_at        INTEGER NOT NULL,
                 updated_at        INTEGER NOT NULL
             );
             CREATE UNIQUE INDEX payments_idempotency
                 ON payments (agent_id, idempotency_key);
             CREATE INDEX payments_state ON payments (state);

             CREATE TABLE transitions (
                 seq        INTEGER PRIMARY KEY AUTOINCREMENT,
                 payment_id TEXT NOT NULL REFERENCES payments (payment_id),
                 from_state TEXT NOT NULL,
                 to_state   TEXT NOT NULL,
                 at         INTEGER NOT NULL,
                 reason     TEXT
             );
             CREATE INDEX transitions_payment ON transitions (payment_id, seq);

             PRAGMA user_version = 1;
             COMMIT;",
        )?;
        version = 1;
    }

    if version == 1 {
        // v2 (ADR 0002/ADR 0007, 2026-11-21): the long-term card vault, and
        // the card the human chose at the approval click.
        connection.execute_batch(
            "BEGIN;
             CREATE TABLE cards (
                 card_id    TEXT PRIMARY KEY,
                 pet_name   TEXT NOT NULL,
                 last4      TEXT NOT NULL CHECK (length(last4) = 4),
                 blob       BLOB NOT NULL,
                 created_at INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL
             );
             CREATE UNIQUE INDEX cards_pet_name ON cards (pet_name);
             ALTER TABLE payments ADD COLUMN chosen_card TEXT REFERENCES cards (card_id);
             PRAGMA user_version = 2;
             COMMIT;",
        )?;
    }

    Ok(())
}

fn decode_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<StoredPayment, StoreError>> {
    let payment_id: String = row.get(0)?;
    let agent_id: String = row.get(1)?;
    let idempotency_key: String = row.get(2)?;
    let state: String = row.get(3)?;
    let cart_json: String = row.get(4)?;
    let tab_id: String = row.get(5)?;
    let fields_json: String = row.get(6)?;
    let submit_selector: String = row.get(7)?;
    let terminal_reason: Option<String> = row.get(8)?;
    let order_reference: Option<String> = row.get(9)?;
    let submit_attempted: i64 = row.get(10)?;
    let submitted_at: Option<i64> = row.get(11)?;
    let chosen_card: Option<String> = row.get(12)?;
    let approval_deadline: i64 = row.get(13)?;
    let created_at: i64 = row.get(14)?;
    let updated_at: i64 = row.get(15)?;

    Ok(decode_payment(DecodedRow {
        payment_id,
        agent_id,
        idempotency_key,
        state,
        cart_json,
        tab_id,
        fields_json,
        submit_selector,
        terminal_reason,
        order_reference,
        submit_attempted,
        submitted_at,
        chosen_card,
        approval_deadline,
        created_at,
        updated_at,
    }))
}

struct DecodedRow {
    payment_id: String,
    agent_id: String,
    idempotency_key: String,
    state: String,
    cart_json: String,
    tab_id: String,
    fields_json: String,
    submit_selector: String,
    terminal_reason: Option<String>,
    order_reference: Option<String>,
    submit_attempted: i64,
    submitted_at: Option<i64>,
    chosen_card: Option<String>,
    approval_deadline: i64,
    created_at: i64,
    updated_at: i64,
}

fn decode_payment(row: DecodedRow) -> Result<StoredPayment, StoreError> {
    let state = PaymentState::parse(&row.state).ok_or(StoreError::Corrupt("unknown state"))?;
    let payment_id: PaymentId = row
        .payment_id
        .parse()
        .map_err(|_| StoreError::Corrupt("malformed payment id"))?;
    // `serde_json::from_str` is used rather than `from_value` so a malformed
    // row is a decode error, not a panic.
    let cart: Cart = serde_json::from_str(&row.cart_json)?;
    let fields: Vec<FieldSpec> = serde_json::from_str(&row.fields_json)?;

    Ok(StoredPayment {
        payment_id,
        agent_id: row.agent_id,
        idempotency_key: row.idempotency_key,
        state,
        cart,
        tab_id: row.tab_id,
        fields,
        submit_selector: row.submit_selector,
        order_reference: row.order_reference.and_then(|raw| {
            // Stored references were validated on write; a bad row degrades to
            // "no reference" instead of failing the read.
            OrderReference::parse(&raw).ok()
        }),
        terminal_reason: row
            .terminal_reason
            .and_then(|raw| TerminalReason::parse(&raw)),
        submit_attempted: row.submit_attempted != 0,
        submitted_at: row.submitted_at,
        // Written from a `CardId`; a bad row degrades to "no card chosen"
        // rather than failing the read (the fill path then aborts cleanly).
        chosen_card: row.chosen_card.and_then(|raw| raw.parse().ok()),
        approval_deadline: row.approval_deadline,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

fn is_unique_violation(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.code == rusqlite::ErrorCode::ConstraintViolation
    )
}

/// Unix seconds, the only timestamp format the schema stores.
#[must_use]
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}
