//! The in-process long-poll registry (ADR 0004).
//!
//! One `tokio::sync::watch` channel per in-flight payment, keyed by payment id.
//! Waiters clone a receiver; the state machine publishes on every transition.
//!
//! The channel is **advisory**: it exists to wake a waiter quickly, not to be
//! the truth. [`crate::engine`] re-reads the row from SQLite after every wake,
//! so a missed notification costs latency, never correctness (ADR 0004:
//! "authoritative-by-DB, not by the channel"). This is also why a single
//! `watch` is enough instead of a broadcast stream: an agent that missed
//! `filling` does not care about it.
//!
//! The registry is single-process by design; a restart drops waiters, and the
//! agent re-polls `GET /payments/:id` (D11).

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use tokio::sync::watch;

use crate::payment::PaymentId;
use crate::state::PaymentState;

/// Per-payment state-change notifications.
#[derive(Debug, Default)]
pub struct WaitRegistry {
    senders: Mutex<HashMap<PaymentId, watch::Sender<PaymentState>>>,
}

impl WaitRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribe a waiter, creating the channel seeded with `current` if this is
    /// the payment's first waiter.
    pub fn subscribe(
        &self,
        payment_id: PaymentId,
        current: PaymentState,
    ) -> watch::Receiver<PaymentState> {
        let mut senders = self.lock();
        senders
            .entry(payment_id)
            .or_insert_with(|| watch::channel(current).0)
            .subscribe()
    }

    /// Publish a state change and drop the channel once the payment is terminal
    /// (ADR 0004: "the registry entry is dropped on terminal state").
    pub fn publish(&self, payment_id: PaymentId, state: PaymentState) {
        let mut senders = self.lock();
        if let Some(sender) = senders.get(&payment_id) {
            sender.send_replace(state);
            if state.is_terminal() {
                senders.remove(&payment_id);
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<PaymentId, watch::Sender<PaymentState>>> {
        // A poisoned registry only means some other task panicked mid-update;
        // the map itself is still usable and dropping it would strand waiters.
        self.senders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
