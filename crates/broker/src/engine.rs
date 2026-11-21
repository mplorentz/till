//! The payment engine: owns every transition, the approval timer, the
//! fill→submit pipeline and the long-poll wait.
//!
//! Everything outside the broker reaches it through three entry points and two
//! traits:
//!
//! * `POST /payments` → [`PaymentEngine::create_payment`]
//! * Approve/Decline/outcome clicks → [`PaymentEngine::approve`],
//!   [`PaymentEngine::decline`], [`PaymentEngine::report_outcome`]
//!   (called in-process by `crates/discord-gw`; see ADR 0006)
//! * Camofox → [`CamofoxPort`], Discord → [`DiscordPort`]
//!
//! # Ordering rules encoded here
//!
//! * The submit-once latch is committed **before** the click is issued
//!   (AGENTS.md rule 3, ADR 0001) — the latch rides on the same `UPDATE` that
//!   moves `filling → submitting`.
//! * A click we cannot confirm yields `ambiguous`, never a retry (spec §7).
//! * A hold expiry or a late click can only ever produce `approval_timeout`,
//!   and no fill has run at that point (US-9).
//! * Fill is only ever requested from the `approved`/`filling` path, so card
//!   values cannot be sent speculatively (AGENTS.md rule 7).

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::{Instant, sleep_until, timeout_at};

use crate::payment::{NewPayment, Outcome, PaymentId, PaymentView, TerminalReason};
use crate::ports::{ApprovalRequest, CamofoxPort, DiscordPort, FillRequest, ResultReviewRequest};
use crate::state::{PaymentEvent, PaymentState, transition};
use crate::store::{SqliteStore, StoreError, StoredPayment, TransitionEffect, now_unix};
use crate::wait::WaitRegistry;

/// Camofox reaps an idle tab after 5 minutes, so a longer approval TTL would
/// approve a payment whose tab is already gone (ADR 0004).
pub const TAB_INACTIVITY_SECONDS: u64 = 300;

/// Engine tuning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineConfig {
    /// How long a human has to Approve/Decline; the merchant hold is treated as
    /// this deadline (D8, US-9).
    pub approval_ttl: Duration,
    /// Upper bound applied to a caller's `?timeout=` (ADR 0004).
    pub max_wait_timeout: Duration,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            approval_ttl: Duration::from_secs(240),
            max_wait_timeout: Duration::from_secs(120),
        }
    }
}

impl EngineConfig {
    /// Build a config, clamping the approval TTL to Camofox's tab-inactivity
    /// window so an approval can never outlive its tab.
    #[must_use]
    pub fn new(approval_ttl: Duration, max_wait_timeout: Duration) -> Self {
        Self {
            approval_ttl: approval_ttl.min(Duration::from_secs(TAB_INACTIVITY_SECONDS)),
            max_wait_timeout,
        }
    }

    /// The approval TTL in whole seconds, floored at 1 so a payment always has
    /// a live approval window.
    #[must_use]
    fn approval_ttl_seconds(self) -> i64 {
        i64::try_from(self.approval_ttl.as_secs().max(1)).unwrap_or(i64::MAX)
    }
}

/// The result of `POST /payments` (US-1, US-13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateOutcome {
    /// The payment id, freshly minted or the one the key already owned.
    pub payment_id: PaymentId,
    /// `false` when this POST was deduped onto an existing payment.
    pub created: bool,
}

/// The result of a human click (spec §6 stale-interaction handling).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionResult {
    /// The click was applied to the state machine.
    Applied,
    /// The payment is not in a state that accepts this click; the gateway must
    /// answer ephemerally and nothing was filled or submitted.
    Expired,
}

/// Anything the engine can refuse.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// The store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The request was not usable. Static text only: agent-supplied strings are
    /// never echoed into an error (AGENTS.md rule 1).
    #[error("invalid payment request: {0}")]
    Invalid(&'static str),
}

/// The engine handle. Cheap to clone; all state sits behind the `Arc`.
#[derive(Debug, Clone)]
pub struct PaymentEngine {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    store: Arc<SqliteStore>,
    camofox: Arc<dyn CamofoxPort>,
    discord: Arc<dyn DiscordPort>,
    config: EngineConfig,
    waiters: WaitRegistry,
}

impl PaymentEngine {
    /// Assemble an engine around a store and the two port implementations.
    #[must_use]
    pub fn new(
        store: Arc<SqliteStore>,
        camofox: Arc<dyn CamofoxPort>,
        discord: Arc<dyn DiscordPort>,
        config: EngineConfig,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                camofox,
                discord,
                config,
                waiters: WaitRegistry::new(),
            }),
        }
    }

    /// The store this engine writes to.
    #[must_use]
    pub fn store(&self) -> &Arc<SqliteStore> {
        &self.inner.store
    }

    /// The active configuration.
    #[must_use]
    pub fn config(&self) -> EngineConfig {
        self.inner.config
    }

    /// The long-poll registry, so the HTTP layer can subscribe.
    #[must_use]
    pub fn waiters(&self) -> &WaitRegistry {
        &self.inner.waiters
    }

    /// Create a payment, or return the one this idempotency key already owns
    /// (US-13).
    ///
    /// # Errors
    ///
    /// [`EngineError::Invalid`] for an unusable request, [`EngineError::Store`]
    /// for persistence failures.
    pub async fn create_payment(
        &self,
        agent_id: &str,
        request: NewPayment,
    ) -> Result<CreateOutcome, EngineError> {
        validate(agent_id, &request)?;

        // Fast path: the key is already claimed, answer with the original id.
        if let Some(existing) = self
            .inner
            .store
            .find_by_idempotency_key(agent_id, &request.idempotency_key)?
        {
            return Ok(CreateOutcome {
                payment_id: existing,
                created: false,
            });
        }

        let deadline = now_unix() + self.inner.config.approval_ttl_seconds();
        let payment_id = match self
            .inner
            .store
            .insert_pending(agent_id, &request, deadline)
        {
            Ok(payment_id) => payment_id,
            // Lost the race on the unique index: the other writer's id wins.
            Err(StoreError::Duplicate(existing)) => {
                return Ok(CreateOutcome {
                    payment_id: existing,
                    created: false,
                });
            }
            Err(error) => return Err(error.into()),
        };

        self.inner
            .waiters
            .subscribe(payment_id, PaymentState::PendingApproval);

        // US-6: ask the gateway to post the approval message. A gateway failure
        // must not fail the POST — the payment exists and its hold timer runs;
        // the human simply will not see a button and the payment will time out.
        if let Err(error) = self
            .inner
            .discord
            .request_approval(ApprovalRequest {
                payment_id,
                cart: &request.cart,
                approval_deadline: deadline,
            })
            .await
        {
            eprintln!("broker: approval request for {payment_id} failed: {error}");
        }

        self.arm_hold_timer(payment_id, deadline);

        Ok(CreateOutcome {
            payment_id,
            created: true,
        })
    }

    /// The human clicked Approve (US-7): authorize the fill and start it.
    ///
    /// Returns [`DecisionResult::Expired`] for a late click, in which case
    /// nothing is filled and nothing is submitted (US-9).
    ///
    /// # Errors
    ///
    /// [`EngineError::Store`] if the payment cannot be read or updated.
    pub fn approve(&self, payment_id: PaymentId) -> Result<DecisionResult, EngineError> {
        let stored = self.inner.store.load(payment_id)?;
        if stored.state != PaymentState::PendingApproval {
            return Ok(DecisionResult::Expired);
        }
        if stored.approval_deadline <= now_unix() {
            // The timer has not fired yet but the hold is over. Close it now so
            // the click cannot authorize a fill (US-9).
            self.finish(
                payment_id,
                PaymentState::PendingApproval,
                PaymentState::ApprovalTimeout,
                TerminalReason::HoldExpired,
            );
            return Ok(DecisionResult::Expired);
        }

        let approved = match self.inner.store.transition(
            payment_id,
            PaymentState::PendingApproval,
            PaymentState::Approved,
            &TransitionEffect::default(),
        ) {
            Ok(stored) => stored,
            // The hold-expiry timer won the race: nothing may be filled.
            Err(StoreError::Conflict { current, .. }) if current.is_terminal() => {
                return Ok(DecisionResult::Expired);
            }
            Err(error) => return Err(error.into()),
        };
        self.inner.waiters.publish(payment_id, approved.state);

        self.spawn_fill_and_submit(approved);
        Ok(DecisionResult::Applied)
    }

    /// The human clicked Decline (US-8): nothing is ever filled.
    ///
    /// # Errors
    ///
    /// [`EngineError::Store`] if the payment cannot be read or updated.
    pub fn decline(&self, payment_id: PaymentId) -> Result<DecisionResult, EngineError> {
        let stored = self.inner.store.load(payment_id)?;
        if stored.state != PaymentState::PendingApproval {
            return Ok(DecisionResult::Expired);
        }

        let applied = self.try_finish(
            payment_id,
            PaymentState::PendingApproval,
            PaymentState::Declined,
            TerminalReason::DeclinedByHuman,
        )?;
        Ok(if applied {
            DecisionResult::Applied
        } else {
            DecisionResult::Expired
        })
    }

    /// The human classified the result screenshot (US-11, spec §6).
    ///
    /// # Errors
    ///
    /// [`EngineError::Store`] if the payment cannot be read or updated.
    pub fn report_outcome(
        &self,
        payment_id: PaymentId,
        outcome: Outcome,
    ) -> Result<DecisionResult, EngineError> {
        let kind = outcome.kind();
        let next = transition(
            PaymentState::AwaitingResultReview,
            PaymentEvent::Outcome(kind),
        );
        let Ok(next) = next else {
            return Ok(DecisionResult::Expired);
        };

        let stored = self.inner.store.load(payment_id)?;
        if stored.state != PaymentState::AwaitingResultReview {
            // A stale interaction on a terminal payment gets "this request has
            // expired" (spec §6), and never re-opens a settled outcome.
            return Ok(DecisionResult::Expired);
        }

        let effect = TransitionEffect {
            terminal_reason: Some(TerminalReason::ResultReviewed),
            order_reference: outcome.order_reference().cloned(),
            submit_attempted: false,
        };
        match self.inner.store.transition(
            payment_id,
            PaymentState::AwaitingResultReview,
            next,
            &effect,
        ) {
            Ok(stored) => {
                self.inner.waiters.publish(payment_id, stored.state);
                Ok(DecisionResult::Applied)
            }
            Err(StoreError::Conflict { .. }) => Ok(DecisionResult::Expired),
            Err(error) => Err(error.into()),
        }
    }

    /// Read a payment (US recovery path, D11). Never contains card data.
    ///
    /// # Errors
    ///
    /// [`EngineError::Store`], including "not found".
    pub fn view(&self, payment_id: PaymentId) -> Result<PaymentView, EngineError> {
        Ok(self.inner.store.load(payment_id)?.view())
    }

    /// Read a payment as a specific agent: another agent's id is a 404.
    ///
    /// Per-agent keys are only a real boundary if a payment is scoped to the
    /// agent that created it (ADR 0006).
    ///
    /// # Errors
    ///
    /// [`EngineError::Store`], including "not found" for a foreign payment.
    pub fn view_for(
        &self,
        agent_id: &str,
        payment_id: PaymentId,
    ) -> Result<PaymentView, EngineError> {
        let stored = self.load_owned(agent_id, payment_id)?;
        Ok(stored.view())
    }

    /// Like [`Self::wait`], but scoped to the agent that owns the payment.
    ///
    /// # Errors
    ///
    /// [`EngineError::Store`], including "not found" for a foreign payment.
    pub async fn wait_for(
        &self,
        agent_id: &str,
        payment_id: PaymentId,
        timeout: Option<Duration>,
    ) -> Result<PaymentView, EngineError> {
        // Ownership is checked before the wait starts; `agent_id` is immutable
        // for the life of a payment, so the loop below needs no re-check.
        self.load_owned(agent_id, payment_id)?;
        self.wait(payment_id, timeout).await
    }

    fn load_owned(
        &self,
        agent_id: &str,
        payment_id: PaymentId,
    ) -> Result<StoredPayment, EngineError> {
        let stored = self.inner.store.load(payment_id)?;
        if stored.agent_id == agent_id {
            Ok(stored)
        } else {
            Err(StoreError::NotFound(payment_id).into())
        }
    }

    /// Long-poll until the state changes or `timeout` elapses (ADR 0004).
    ///
    /// The database is authoritative: every wake re-reads the row, so a missed
    /// notification only costs latency. On timeout the current non-terminal
    /// state is returned — that is a success, not an error, so the agent can
    /// re-invoke cheaply.
    ///
    /// # Errors
    ///
    /// [`EngineError::Store`], including "not found".
    pub async fn wait(
        &self,
        payment_id: PaymentId,
        timeout: Option<Duration>,
    ) -> Result<PaymentView, EngineError> {
        let budget = timeout
            .unwrap_or(self.inner.config.max_wait_timeout)
            .min(self.inner.config.max_wait_timeout);
        let deadline = Instant::now() + budget;

        let current = self.inner.store.load(payment_id)?;
        if current.state.is_terminal() {
            return Ok(current.view());
        }
        let mut receiver: watch::Receiver<PaymentState> =
            self.inner.waiters.subscribe(payment_id, current.state);

        loop {
            let stored: StoredPayment = self.inner.store.load(payment_id)?;
            if stored.state.is_terminal() {
                return Ok(stored.view());
            }

            match timeout_at(deadline, receiver.changed()).await {
                // Something changed: re-read the row and re-check.
                Ok(Ok(())) => {}
                // The channel closed (the payment went terminal elsewhere).
                Ok(Err(_)) => return Ok(stored.view()),
                // The caller's budget is spent: hand back the current state.
                Err(_) => return Ok(stored.view()),
            }
        }
    }

    /// Bring the store to a consistent post-crash state (US-12).
    ///
    /// * A pending approval whose hold already expired → `approval_timeout`.
    /// * A pending approval still inside its window → the timer is re-armed
    ///   (the original task died with the process).
    /// * `approved`/`filling` with no latch → `aborted_before_submit`: the
    ///   process died before anything was clicked.
    /// * anything latched and not yet terminal → `ambiguous`: we cannot prove
    ///   whether the click landed (ADR 0001).
    /// * `awaiting_result_review` → left alone; it is waiting on a human, not
    ///   on a timer or a tab.
    ///
    /// # Errors
    ///
    /// [`EngineError::Store`] if the in-flight set cannot be read.
    pub fn recover(&self) -> Result<(), EngineError> {
        for stored in self.inner.store.in_flight()? {
            let payment_id = stored.payment_id;
            match stored.state {
                PaymentState::PendingApproval => {
                    if stored.approval_deadline > now_unix() {
                        self.inner
                            .waiters
                            .subscribe(payment_id, PaymentState::PendingApproval);
                        self.arm_hold_timer(payment_id, stored.approval_deadline);
                    } else {
                        self.finish(
                            payment_id,
                            PaymentState::PendingApproval,
                            PaymentState::ApprovalTimeout,
                            TerminalReason::HoldExpired,
                        );
                    }
                }
                PaymentState::AwaitingResultReview => {}
                PaymentState::Approved | PaymentState::Filling | PaymentState::Submitting => {
                    if stored.submit_attempted {
                        self.finish(
                            payment_id,
                            stored.state,
                            PaymentState::Ambiguous,
                            TerminalReason::CrashRecovery,
                        );
                    } else {
                        self.finish(
                            payment_id,
                            stored.state,
                            PaymentState::AbortedBeforeSubmit,
                            TerminalReason::CrashRecovery,
                        );
                    }
                }
                // Terminal states are not in flight; nothing to do.
                _ => {}
            }
        }
        Ok(())
    }

    /// Arm (or re-arm) the hold-expiry timer for one payment.
    fn arm_hold_timer(&self, payment_id: PaymentId, deadline_unix: i64) {
        let engine = self.clone();
        tokio::spawn(async move {
            let remaining = deadline_unix.saturating_sub(now_unix());
            let remaining = Duration::from_secs(u64::try_from(remaining).unwrap_or_default());
            sleep_until(Instant::now() + remaining).await;
            engine.expire_hold(payment_id);
        });
    }

    /// The hold timer fired: expire the approval if it is still pending.
    fn expire_hold(&self, payment_id: PaymentId) {
        // A payment the human already decided on is not pending any more; the
        // compare-and-set below simply finds nothing to change.
        self.finish(
            payment_id,
            PaymentState::PendingApproval,
            PaymentState::ApprovalTimeout,
            TerminalReason::HoldExpired,
        );
    }

    /// Spawn the fill → submit pipeline so the Approve click returns at once.
    fn spawn_fill_and_submit(&self, stored: StoredPayment) {
        let engine = self.clone();
        tokio::spawn(async move {
            engine.run_fill_and_submit(stored).await;
        });
    }

    /// approved → filling → (fill) → submitting (latch) → (click) →
    /// awaiting_result_review.
    async fn run_fill_and_submit(&self, stored: StoredPayment) {
        let payment_id = stored.payment_id;

        // Post-approval only: this is the first moment a fill is legal.
        if !self.advance(
            payment_id,
            PaymentState::Approved,
            PaymentState::Filling,
            TransitionEffect::default(),
        ) {
            return;
        }

        let filled = self
            .inner
            .camofox
            .fill_fields(FillRequest {
                payment_id,
                tab_id: &stored.tab_id,
                fields: &stored.fields,
            })
            .await;

        if filled.is_err() {
            // Pre-submit error: nothing was clicked, so nothing charged.
            self.finish(
                payment_id,
                PaymentState::Filling,
                PaymentState::AbortedBeforeSubmit,
                TerminalReason::PreSubmitError,
            );
            return;
        }

        // Commit the submit-once latch *before* the click, in the same
        // transition that enters `submitting` (AGENTS.md rule 3, ADR 0001).
        if !self.advance(
            payment_id,
            PaymentState::Filling,
            PaymentState::Submitting,
            TransitionEffect {
                submit_attempted: true,
                ..TransitionEffect::default()
            },
        ) {
            return;
        }

        let clicked = self
            .inner
            .camofox
            .click_submit(payment_id, &stored.tab_id, &stored.submit_selector)
            .await;

        if clicked.is_err() {
            // We cannot prove whether the click landed: ambiguous, never retry.
            self.finish(
                payment_id,
                PaymentState::Submitting,
                PaymentState::Ambiguous,
                TerminalReason::UnconfirmedSubmit,
            );
            return;
        }

        if !self.advance(
            payment_id,
            PaymentState::Submitting,
            PaymentState::AwaitingResultReview,
            TransitionEffect::default(),
        ) {
            return;
        }

        // US-10: hand the human a result screenshot to classify.
        if let Err(error) = self
            .inner
            .discord
            .request_result_review(ResultReviewRequest {
                payment_id,
                cart: &stored.cart,
            })
            .await
        {
            eprintln!("broker: result review for {payment_id} failed: {error}");
        }
    }

    /// Apply one transition and publish it; `false` if another actor got there
    /// first (nothing is published in that case).
    fn advance(
        &self,
        payment_id: PaymentId,
        from: PaymentState,
        to: PaymentState,
        effect: TransitionEffect,
    ) -> bool {
        match self.inner.store.transition(payment_id, from, to, &effect) {
            Ok(stored) => {
                self.inner.waiters.publish(payment_id, stored.state);
                true
            }
            Err(error) => {
                // Conflicts mean the state machine already moved on (another
                // click, the hold timer, a restart): safe to ignore.
                if !matches!(error, StoreError::Conflict { .. }) {
                    eprintln!("broker: transition {payment_id} {from:?}->{to:?} failed: {error}");
                }
                false
            }
        }
    }

    /// Apply a terminal transition, ignoring conflicts.
    fn finish(
        &self,
        payment_id: PaymentId,
        from: PaymentState,
        to: PaymentState,
        reason: TerminalReason,
    ) -> bool {
        self.advance(
            payment_id,
            from,
            to,
            TransitionEffect {
                terminal_reason: Some(reason),
                ..TransitionEffect::default()
            },
        )
    }

    /// Like [`Self::finish`], but reports whether the state machine accepted it.
    fn try_finish(
        &self,
        payment_id: PaymentId,
        from: PaymentState,
        to: PaymentState,
        reason: TerminalReason,
    ) -> Result<bool, EngineError> {
        match self.inner.store.transition(
            payment_id,
            from,
            to,
            &TransitionEffect {
                terminal_reason: Some(reason),
                ..TransitionEffect::default()
            },
        ) {
            Ok(stored) => {
                self.inner.waiters.publish(payment_id, stored.state);
                Ok(true)
            }
            Err(StoreError::Conflict { .. }) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }
}

/// Reject a request before anything is persisted.
///
/// The error strings are static: the agent's own text is never echoed back into
/// an error, an error page or a log line.
fn validate(agent_id: &str, request: &NewPayment) -> Result<(), EngineError> {
    if agent_id.trim().is_empty() {
        return Err(EngineError::Invalid("agent id must not be empty"));
    }
    let key = request.idempotency_key.trim();
    if key.is_empty() || key.len() > 128 {
        return Err(EngineError::Invalid(
            "idempotency_key must be 1..=128 characters",
        ));
    }
    if request.tab_id.trim().is_empty() {
        return Err(EngineError::Invalid("tab_id must not be empty"));
    }
    if request.submit_selector.trim().is_empty() {
        return Err(EngineError::Invalid("submit_selector must not be empty"));
    }
    if request.fields.is_empty() {
        return Err(EngineError::Invalid("fields must not be empty"));
    }
    if request
        .fields
        .iter()
        .any(|field| field.selector.trim().is_empty())
    {
        return Err(EngineError::Invalid("field selectors must not be empty"));
    }
    let origin = request.cart.merchant_origin.trim();
    if !(origin.starts_with("https://") || origin.starts_with("http://")) {
        return Err(EngineError::Invalid(
            "cart.merchant_origin must be an http(s) origin",
        ));
    }
    if request.cart.total.trim().is_empty() {
        return Err(EngineError::Invalid("cart.total must not be empty"));
    }
    if request.cart.currency.trim().is_empty() {
        return Err(EngineError::Invalid("cart.currency must not be empty"));
    }
    Ok(())
}
