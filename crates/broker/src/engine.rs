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
//! * The card is chosen by the **human** at the approval click, and the choice
//!   is committed in the same transition as the approval (ADR 0007). Fill is
//!   only ever requested from the approved path, so card values cannot be
//!   sent speculatively (AGENTS.md rule 7).
//! * Decryption happens once, in this fill path, and the plaintext is dropped
//!   as soon as the Camofox fill call returns (ADR 0002).

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::{Instant, sleep_until, timeout_at};

use crate::artifacts::{ArtifactKind, ScreenshotStore};
use crate::crypto::CardCipher;
use crate::payment::{CardId, NewPayment, Outcome, PaymentId, PaymentView, TerminalReason};
use crate::ports::{ApprovalRequest, CamofoxPort, DiscordPort, FillRequest, ResultReviewRequest};
use crate::state::{PaymentEvent, PaymentState};
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
    /// The click cannot be honoured as sent — no card is registered, several
    /// are and none was named, or the named card does not exist. The gateway
    /// answers ephemerally ("pick a card") and nothing was filled or
    /// submitted. Distinct from [`Self::Expired`] so the human is told the
    /// truth (ADR 0007).
    Refused,
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
    cipher: Arc<CardCipher>,
    artifacts: Arc<ScreenshotStore>,
    config: EngineConfig,
    waiters: WaitRegistry,
}

impl PaymentEngine {
    /// Assemble an engine around a store, the two port implementations, the
    /// deployment's card cipher and its artifact directory.
    #[must_use]
    pub fn new(
        store: Arc<SqliteStore>,
        camofox: Arc<dyn CamofoxPort>,
        discord: Arc<dyn DiscordPort>,
        cipher: Arc<CardCipher>,
        artifacts: Arc<ScreenshotStore>,
        config: EngineConfig,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                camofox,
                discord,
                cipher,
                artifacts,
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

    /// The artifact store masked screenshots are written to (ADR 0008).
    #[must_use]
    pub fn artifacts(&self) -> &Arc<ScreenshotStore> {
        &self.inner.artifacts
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

        // The approval message must carry the card listing, and this read must
        // happen **before** anything is persisted: a failure here would
        // otherwise leave a payment that exists but never got its hold timer
        // armed (a gateway-side failure must never fail the POST — US-6).
        let cards = self.inner.store.cards()?;

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
                cards: &cards,
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
    /// `card` is the card the human picked in the approval message. With
    /// exactly one card registered the gateway may send `None` and the engine
    /// auto-selects it (overseer requirement 2026-11-21); with several, a
    /// choice is required — the engine never guesses which card pays.
    ///
    /// The choice is committed in the **same** transition as the approval, so
    /// "approved" and "paying with this card" can never disagree.
    ///
    /// Returns [`DecisionResult::Expired`] for a late click (US-9) and
    /// [`DecisionResult::Refused`] when the click cannot name a usable card;
    /// in neither case is anything filled or submitted.
    ///
    /// # Errors
    ///
    /// [`EngineError::Store`] if the payment cannot be read or updated.
    pub fn approve(
        &self,
        payment_id: PaymentId,
        card: Option<CardId>,
    ) -> Result<DecisionResult, EngineError> {
        let stored = self.inner.store.load(payment_id)?;
        if stored.state != PaymentState::PendingApproval {
            return Ok(DecisionResult::Expired);
        }
        if self.hold_over(&stored) {
            return Ok(DecisionResult::Expired);
        }

        let Some(chosen) = self.resolve_card_choice(card)? else {
            return Ok(DecisionResult::Refused);
        };

        let approved = match self.inner.store.transition(
            payment_id,
            PaymentState::PendingApproval,
            PaymentEvent::Approve,
            &TransitionEffect {
                chosen_card: Some(chosen),
                ..TransitionEffect::default()
            },
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

    /// Turn the click's card (or the absence of one) into the card to pay with.
    ///
    /// `Some(id)` must be an **active** card; `None` is only legal when exactly
    /// one card is active, in which case that card is selected. `None`
    /// otherwise means "refuse" — nothing is filled (ADR 0007). A retired card
    /// is never offered, so it is never chosen.
    fn resolve_card_choice(&self, card: Option<CardId>) -> Result<Option<CardId>, StoreError> {
        if let Some(card_id) = card {
            return Ok(self
                .inner
                .store
                .active_card(card_id)?
                .map(|card| card.card_id));
        }

        let cards = self.inner.store.cards()?;
        match cards.as_slice() {
            [only] => Ok(Some(only.card_id)),
            _ => Ok(None),
        }
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
        // A click after the hold elapsed is late whichever button it is: the
        // payment is closed as `approval_timeout`, not `declined`, so both
        // buttons behave identically (US-9).
        if self.hold_over(&stored) {
            return Ok(DecisionResult::Expired);
        }

        let applied = self.try_finish(
            payment_id,
            PaymentState::PendingApproval,
            PaymentEvent::Decline,
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
        let event = PaymentEvent::Outcome(outcome.kind());
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
            chosen_card: None,
        };
        match self.inner.store.transition(
            payment_id,
            PaymentState::AwaitingResultReview,
            event,
            &effect,
        ) {
            Ok(stored) => {
                self.inner.waiters.publish(payment_id, stored.state);
                // spec §7: after a terminal outcome the tab is released to the
                // agent only as `about:blank`, so card values cannot be read
                // back out of the page.
                self.spawn_containment(payment_id, &stored.tab_id);
                Ok(DecisionResult::Applied)
            }
            Err(StoreError::Conflict { .. } | StoreError::Illegal(_)) => {
                Ok(DecisionResult::Expired)
            }
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
                            PaymentEvent::HoldExpired,
                            TerminalReason::HoldExpired,
                        );
                    }
                }
                PaymentState::AwaitingResultReview => {}
                PaymentState::Approved | PaymentState::Filling | PaymentState::Submitting => {
                    // One event: the table decides whether this is
                    // `ambiguous` (latched, cannot prove the click) or
                    // `aborted_before_submit` (nothing was ever sent).
                    self.finish(
                        payment_id,
                        stored.state,
                        PaymentEvent::CrashRecovered,
                        TerminalReason::CrashRecovery,
                    );
                    // The tab may still hold card values after a crash: contain
                    // it best-effort (spec §7). A tab that is already gone is
                    // fine — the payment is terminal either way.
                    if let Ok(handle) = tokio::runtime::Handle::try_current() {
                        let camofox = Arc::clone(&self.inner.camofox);
                        let tab_id = stored.tab_id.clone();
                        handle.spawn(async move {
                            if camofox.contain_tab(payment_id, &tab_id).await.is_err() {
                                eprintln!("broker: containment for {payment_id} failed");
                            }
                        });
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
            PaymentEvent::HoldExpired,
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

    /// approved → filling → (decrypt + fill) → submitting (latch) → (click) →
    /// awaiting_result_review.
    async fn run_fill_and_submit(&self, stored: StoredPayment) {
        let payment_id = stored.payment_id;

        // Post-approval only: this is the first moment a fill is legal.
        if !self.advance(
            payment_id,
            PaymentState::Approved,
            PaymentEvent::FillStarted,
            TransitionEffect::default(),
        ) {
            return;
        }

        // The card the human chose at the approval click. Without one there is
        // nothing to type, and nothing has been submitted yet.
        let Some(chosen) = stored.chosen_card else {
            self.finish(
                payment_id,
                PaymentState::Filling,
                PaymentEvent::FillFailed,
                TerminalReason::PreSubmitError,
            );
            return;
        };

        let blob = match self.inner.store.card_blob(chosen) {
            Ok(Some(blob)) => blob,
            Ok(None) | Err(_) => {
                self.finish(
                    payment_id,
                    PaymentState::Filling,
                    PaymentEvent::FillFailed,
                    TerminalReason::PreSubmitError,
                );
                return;
            }
        };

        // The one decryption in the broker (ADR 0002, AGENTS.md rule 7).
        // `card` is dropped — and zeroized — the moment the fill returns.
        let card = match self.inner.cipher.decrypt(&blob) {
            Ok(card) => card,
            Err(_) => {
                self.finish(
                    payment_id,
                    PaymentState::Filling,
                    PaymentEvent::FillFailed,
                    TerminalReason::PreSubmitError,
                );
                return;
            }
        };

        let filled = self
            .inner
            .camofox
            .fill_fields(FillRequest {
                payment_id,
                tab_id: &stored.tab_id,
                fields: &stored.fields,
                card: &card,
            })
            .await;
        // Plaintext exists only across the fill call above.
        drop(card);

        if filled.is_err() {
            // Pre-submit error: nothing was clicked, so nothing charged.
            self.finish(
                payment_id,
                PaymentState::Filling,
                PaymentEvent::FillFailed,
                TerminalReason::PreSubmitError,
            );
            // A partially typed form still holds card values: contain the tab
            // before the agent gets it back (spec §7 structural property).
            self.spawn_containment(payment_id, &stored.tab_id);
            return;
        }

        // Commit the submit-once latch *before* the click, in the same
        // transition that enters `submitting` (AGENTS.md rule 3, ADR 0001).
        if !self.advance(
            payment_id,
            PaymentState::Filling,
            PaymentEvent::SubmitAttempted,
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
                PaymentEvent::ClickUnconfirmed,
                TerminalReason::UnconfirmedSubmit,
            );
            // A tab that still holds card values must not be handed back to the
            // agent, whatever the outcome (spec §7).
            self.spawn_containment(payment_id, &stored.tab_id);
            return;
        }

        if !self.advance(
            payment_id,
            PaymentState::Submitting,
            PaymentEvent::SubmitCompleted,
            TransitionEffect::default(),
        ) {
            return;
        }

        // US-10, spec §7: the click is proven once, so from here on nothing can
        // make the payment `ambiguous`. Let the page settle, then clear/mask the
        // payment fields before anything is captured (AGENTS.md rule 4) and
        // leave the masked artifact where the gateway can read it (ADR 0008).
        // Every failure below is best-effort and logged without a payload.
        if self
            .inner
            .camofox
            .wait_for_settle(payment_id, &stored.tab_id)
            .await
            .is_err()
        {
            eprintln!("broker: page settle for {payment_id} failed");
        }

        match self
            .inner
            .camofox
            .capture_masked_result(payment_id, &stored.tab_id, &stored.fields)
            .await
        {
            Ok(png) => {
                if let Err(error) =
                    self.inner
                        .artifacts
                        .store(payment_id, ArtifactKind::Result, &png)
                {
                    eprintln!("broker: result artifact for {payment_id} failed: {error}");
                }
            }
            // The payment stays `awaiting_result_review`: a capture failure
            // cannot un-prove a click that already landed.
            Err(_) => eprintln!("broker: result capture for {payment_id} failed"),
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

    /// Best-effort `about:blank` containment after a terminal shape we can still
    /// reach (spec §7). Fire-and-forget, like the hold timer: a dead tab must
    /// never fail a settled payment.
    fn spawn_containment(&self, payment_id: PaymentId, tab_id: &str) {
        let camofox = Arc::clone(&self.inner.camofox);
        let tab_id = tab_id.to_string();
        tokio::spawn(async move {
            if camofox.contain_tab(payment_id, &tab_id).await.is_err() {
                eprintln!("broker: containment for {payment_id} failed");
            }
        });
    }

    /// Apply one transition and publish it; `false` if another actor got there
    /// first (nothing is published in that case).
    fn advance(
        &self,
        payment_id: PaymentId,
        from: PaymentState,
        event: PaymentEvent,
        effect: TransitionEffect,
    ) -> bool {
        match self
            .inner
            .store
            .transition(payment_id, from, event, &effect)
        {
            Ok(stored) => {
                self.inner.waiters.publish(payment_id, stored.state);
                true
            }
            Err(error) => {
                // Conflicts mean the state machine already moved on (another
                // click, the hold timer, a restart): safe to ignore. An
                // `Illegal` here would be a broker bug in the calling path.
                if !matches!(error, StoreError::Conflict { .. }) {
                    eprintln!(
                        "broker: transition {payment_id} {from:?} on {event:?} failed: {error}"
                    );
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
        event: PaymentEvent,
        reason: TerminalReason,
    ) -> bool {
        self.advance(
            payment_id,
            from,
            event,
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
        event: PaymentEvent,
        reason: TerminalReason,
    ) -> Result<bool, EngineError> {
        match self.inner.store.transition(
            payment_id,
            from,
            event,
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

    /// Whether the human's window has closed, closing the payment if so.
    ///
    /// Both clicks share this: a late click of either kind is refused, and the
    /// payment is recorded as `approval_timeout` rather than left pending until
    /// the timer task gets around to it (US-9). Nothing is filled.
    fn hold_over(&self, stored: &StoredPayment) -> bool {
        if stored.approval_deadline > now_unix() {
            return false;
        }
        self.finish(
            stored.payment_id,
            PaymentState::PendingApproval,
            PaymentEvent::HoldExpired,
            TerminalReason::HoldExpired,
        );
        true
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
