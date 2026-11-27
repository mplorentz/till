//! The supervised fill → submit → settle → mask → capture → contain engine
//! (docs/SPEC.md §7; ADR 0005; ADR 0008).
//!
//! This is the production [`CamofoxPort`]. The *order* of these operations is
//! the broker's business — the engine's state machine drives it — so this type
//! only implements each step, plus the three properties the port promises:
//!
//! 1. **Fill is one supervised pass over `/type`, never `evaluate`.**
//!    [`CamofoxFillEngine::fill_fields`] walks the payment's fields once, in
//!    order, and stops at the first failure. It never retries: a second pass
//!    would be a second set of fill operations for one payment (ADR 0005
//!    Decision 2.1). On camofox ≥ 1.16 each `tab:type` bus event carries a
//!    length, not the text.
//! 2. **Capture never precedes masking.** [`CamofoxPort::capture_masked_result`]
//!    is the only method that returns pixels, and it clears the fields first
//!    (US-10, AGENTS.md rule 4).
//! 3. **Containment is `about:blank` via `evaluate`**, because `/navigate`
//!    rejects it (spike finding B).
//!
//! `/wait` is never used anywhere in this crate (spike finding C).

use std::time::Duration;

use broker::{
    BoxFuture, CamofoxPort, CardFields, FieldKind, FieldSpec, FillRequest, PaymentId, PortError,
};
use tokio::time::{Instant, sleep};

use crate::client::{CamofoxClient, CamofoxError};
use crate::js;

/// How long the page gets to settle after the submit click, and how often the
/// probe runs. Bounded on the broker side because camofox's own `/wait` is
/// unusable near a submit button (spike finding C).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettleConfig {
    /// Total time allowed for the page to look settled.
    pub timeout: Duration,
    /// Gap between probe evaluations.
    pub interval: Duration,
}

impl Default for SettleConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(10),
            interval: Duration::from_millis(200),
        }
    }
}

impl SettleConfig {
    /// A configuration with an explicit bound and poll interval.
    #[must_use]
    pub fn new(timeout: Duration, interval: Duration) -> Self {
        Self { timeout, interval }
    }
}

/// The production [`CamofoxPort`]: fills, submits, masks, captures and contains
/// through a [`CamofoxClient`].
#[derive(Debug, Clone)]
pub struct CamofoxFillEngine {
    client: CamofoxClient,
    settle: SettleConfig,
}

impl CamofoxFillEngine {
    /// Wrap a client with the default settle bound.
    #[must_use]
    pub fn new(client: CamofoxClient) -> Self {
        Self {
            client,
            settle: SettleConfig::default(),
        }
    }

    /// Replace the settle bound (used by the end-to-end harness).
    #[must_use]
    pub fn with_settle(mut self, settle: SettleConfig) -> Self {
        self.settle = settle;
        self
    }

    /// The client underneath, for the harness (tab creation, health).
    #[must_use]
    pub fn client(&self) -> &CamofoxClient {
        &self.client
    }
}

/// Map one agent-declared field kind onto the value the vault holds for it.
///
/// The agent only ever names *selectors* and *kinds*; the broker supplies the
/// value (spec §5). Keeping the mapping in one place is what makes "the fill
/// sends exactly the card the human approved" reviewable in five lines.
fn value_for<'a>(kind: FieldKind, card: &'a CardFields<'a>) -> &'a str {
    match kind {
        FieldKind::CardNumber => card.card_number,
        FieldKind::Expiry => card.expiry,
        FieldKind::Cvv => card.cvv,
        FieldKind::CardholderName => card.cardholder_name,
        FieldKind::BillingPostcode => card.billing_postcode,
    }
}

/// Selectors of every field the broker is allowed to have typed into.
fn selectors(fields: &[FieldSpec]) -> Vec<String> {
    fields.iter().map(|field| field.selector.clone()).collect()
}

fn port(error: CamofoxError) -> PortError {
    // Deliberately discards the kind: `PortError` carries no payload at all, so
    // a port failure can never become a log line with page content in it.
    let _ = error;
    PortError
}

impl CamofoxPort for CamofoxFillEngine {
    fn fill_fields<'a>(&'a self, request: FillRequest<'a>) -> BoxFuture<'a, Result<(), PortError>> {
        Box::pin(async move {
            // The human-approved card, borrowed for exactly this pass: nothing
            // is copied, and the caller drops the record on return.
            let card = request.card.fields();

            // One pass, sequential, no retry. A failure aborts the whole fill
            // so the engine can go `aborted_before_submit` rather than submit a
            // half-typed form (engine.rs `FillFailed`).
            for field in request.fields {
                let value = value_for(field.field_kind, &card);
                self.client
                    .type_into(request.tab_id, &field.selector, value)
                    .await
                    .map_err(port)?;
            }
            Ok(())
        })
    }

    fn click_submit<'a>(
        &'a self,
        _payment_id: PaymentId,
        tab_id: &'a str,
        submit_selector: &'a str,
    ) -> BoxFuture<'a, Result<(), PortError>> {
        Box::pin(async move {
            self.client
                .click(tab_id, submit_selector)
                .await
                .map_err(port)
        })
    }

    fn wait_for_settle<'a>(
        &'a self,
        _payment_id: PaymentId,
        tab_id: &'a str,
    ) -> BoxFuture<'a, Result<(), PortError>> {
        Box::pin(async move {
            let probe = js::settle_probe();
            let deadline = Instant::now() + self.settle.timeout;
            loop {
                // The probe answers a boolean; `true` means "settled". A probe
                // that errors is not fatal on its own — the tab may be
                // mid-navigation — so retry until the bound, then give up.
                let settled = matches!(
                    self.client.evaluate_digit_free(tab_id, &probe).await,
                    Ok(ref value) if value.as_bool() == Some(true)
                );
                if settled {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err(port(CamofoxError::SettleTimeout));
                }
                sleep(self.settle.interval).await;
            }
        })
    }

    fn capture_masked_result<'a>(
        &'a self,
        _payment_id: PaymentId,
        tab_id: &'a str,
        fields: &'a [FieldSpec],
    ) -> BoxFuture<'a, Result<Vec<u8>, PortError>> {
        Box::pin(async move {
            // Mask first: the image that leaves the broker must not contain a
            // card value (US-10, AGENTS.md rule 4). The expression carries
            // selectors only, so it is safe to evaluate while the values are
            // still in the DOM (ADR 0005).
            let clear = js::clear_fields(&selectors(fields));
            self.client
                .evaluate_digit_free(tab_id, &clear)
                .await
                .map_err(port)?;

            self.client.screenshot(tab_id).await.map_err(port)
        })
    }

    fn contain_tab<'a>(
        &'a self,
        _payment_id: PaymentId,
        tab_id: &'a str,
    ) -> BoxFuture<'a, Result<(), PortError>> {
        Box::pin(async move {
            // `/navigate` rejects `about:blank` (spike finding B), so
            // containment is the one navigation that goes through `evaluate`.
            self.client
                .evaluate_digit_free(tab_id, &js::contain_about_blank())
                .await
                .map_err(port)?;
            Ok(())
        })
    }
}
