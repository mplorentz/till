//! Durable payment state in SQLite (ADR 0001).
//!
//! Why the database and not a map: the broker's obligations here are
//! *durability* obligations, not request-scoped ones — the submit-once latch
//! must survive a crash (spec §7, US-12), idempotency must dedupe across
//! restarts (US-13), and a payment found in flight at boot must be recoverable
//! without any in-memory history (US-12).
//!
//! Schema notes (ADR 0001):
//!
//! * `payments` — one row per payment. `(agent_id, idempotency_key)` carries a
//!   `UNIQUE` index, so dedupe is enforced by SQLite rather than by a
//!   read-then-write race in application code.
//! * `transitions` — append-only history `(payment_id, from_state, to_state,
//!   at, reason)`. The state machine is the product, so its history is
//!   queryable with `sqlite3` during an incident.
//! * `submit_attempted` / `submitted_at` — the submit-once latch, written and
//!   committed *before* the click is issued (AGENTS.md rule 3).
//!
//! Deferred to later beads, on purpose: `card_blobs` and the `session_key`
//! column arrive with the fill/Camofox wiring (ADR 0002) — no card value
//! exists anywhere in this bead's schema, because no card value ever reaches
//! these three endpoints.
//!
//! Concurrency: one write connection behind a mutex (writes are serialized,
//! transitions are short) and a second connection for reads, so a long-poll
//! waiter never holds the write connection (ADR 0001, "Bad / accepted costs").
//! No lock is held across an `await`; every helper returns owned values.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};

use crate::payment::{
    Cart, FieldSpec, NewPayment, OrderReference, PaymentId, PaymentView, TerminalReason,
};
use crate::state::PaymentState;

/// Current schema version (`PRAGMA user_version`).
const SCHEMA_VERSION: i64 = 1;

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
    /// The idempotency key is already used by this agent.
    #[error("duplicate idempotency key for payment {0}")]
    Duplicate(PaymentId),
    /// A stored row is unreadable.
    #[error("stored payment is corrupt: {0}")]
    Corrupt(&'static str),
    /// The database mutex was poisoned by a panicking writer.
    #[error("payment store is unavailable")]
    Unavailable,
    /// A schema this build does not understand.
    #[error("unsupported schema version {0}")]
    UnsupportedSchema(i64),
}

/// The SQLite-backed payment store.
#[derive(Debug)]
pub struct SqliteStore {
    writer: Mutex<Connection>,
    reader: Mutex<Connection>,
    path: PathBuf,
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
            path,
        })
    }

    /// The database file this store owns.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
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
    /// The `WHERE state = expected` clause is what makes the state machine
    /// authoritative under concurrency: a hold-expiry timer racing an Approve
    /// click produces exactly one winner, and the loser gets
    /// [`StoreError::Conflict`].
    pub fn transition(
        &self,
        payment_id: PaymentId,
        expected: PaymentState,
        next: PaymentState,
        effect: &TransitionEffect,
    ) -> Result<StoredPayment, StoreError> {
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
                 submitted_at = COALESCE(submitted_at, ?8)
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
     submitted_at, approval_deadline, created_at, updated_at";

fn migrate(connection: &Connection) -> Result<(), StoreError> {
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version == SCHEMA_VERSION {
        return Ok(());
    }
    if version != 0 {
        return Err(StoreError::UnsupportedSchema(version));
    }

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
    let approval_deadline: i64 = row.get(12)?;
    let created_at: i64 = row.get(13)?;
    let updated_at: i64 = row.get(14)?;

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
