//! Server-rendered HTML for the mock checkout site (spec §9).
//!
//! The form field `id`/`name` values are exactly the spec `field_kinds`
//! (`docs/SPEC.md` §5) so a broker test can drive the same selectors it would
//! use on a real merchant: `card_number`, `expiry`, `cvv`,
//! `cardholder_name`, `billing_postcode`.
//!
//! No page ever renders a submitted value back to the client.

use crate::mode::Mode;

/// The spec `field_kinds`, in spec order (`docs/SPEC.md` §5).
pub const FIELD_KINDS: [&str; 5] = [
    "card_number",
    "expiry",
    "cvv",
    "cardholder_name",
    "billing_postcode",
];

/// Human-readable label for each field kind.
fn label(field_kind: &str) -> &'static str {
    match field_kind {
        "card_number" => "Card number",
        "expiry" => "Expiry (MM/YY)",
        "cvv" => "Security code",
        "cardholder_name" => "Name on card",
        "billing_postcode" => "Billing postcode",
        _ => "Field",
    }
}

/// Autocomplete hint for each field kind, so a browser (or Camofox) treats
/// the inputs like a real checkout form.
fn autocomplete(field_kind: &str) -> &'static str {
    match field_kind {
        "card_number" => "cc-number",
        "expiry" => "cc-exp",
        "cvv" => "cc-csc",
        "cardholder_name" => "cc-name",
        "billing_postcode" => "postal-code",
        _ => "off",
    }
}

/// The checkout form, as served by `GET /checkout`.
///
/// `action` already carries the mode (and, for `short_hold`, the hold token
/// and TTL) as query parameters, so the form itself submits no hidden state
/// beyond the card fields.
pub fn checkout_form(mode: Mode, action: &str, hold_notice: Option<&str>) -> String {
    let fields = FIELD_KINDS
        .iter()
        .map(|field_kind| {
            format!(
                r#"      <p class="field">
        <label for="{id}">{label}</label>
        <input id="{id}" name="{id}" type="text" autocomplete="{autocomplete}" required>
      </p>"#,
                id = field_kind,
                label = label(field_kind),
                autocomplete = autocomplete(field_kind),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let hold = hold_notice
        .map(|notice| format!(r#"    <p id="hold-notice">{notice}</p>"#))
        .unwrap_or_default();

    format!(
        r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8">
    <title>Mock checkout — {mode}</title>
  </head>
  <body>
    <h1>Mock checkout</h1>
    <p id="mode">Mode: <strong>{mode}</strong></p>
{hold}
    <form id="checkout" method="post" action="{action}">
{fields}
      <p class="submit">
        <button id="submit" type="submit">Pay now</button>
      </p>
    </form>
  </body>
</html>
"#
    )
}

/// `success` result: an order reference.
pub fn order_confirmed(reference: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
  <head><meta charset="utf-8"><title>Order confirmed</title></head>
  <body>
    <h1>Order confirmed</h1>
    <p id="order-reference">Order reference: <strong>{reference}</strong></p>
  </body>
</html>
"#
    )
}

/// `declined` result: the card was refused, nothing was charged.
pub fn card_declined() -> String {
    r#"<!doctype html>
<html lang="en">
  <head><meta charset="utf-8"><title>Payment failed</title></head>
  <body>
    <h1>Payment failed</h1>
    <p id="card-declined">Your card was declined. No charge was made.</p>
  </body>
</html>
"#
    .to_string()
}

/// `ambiguous` result: a spinner that never resolves.
pub fn processing() -> String {
    r#"<!doctype html>
<html lang="en">
  <head><meta charset="utf-8"><title>Processing…</title></head>
  <body>
    <h1>Processing…</h1>
    <div id="processing-spinner" role="status">Processing your payment. Do not close this window.</div>
  </body>
</html>
"#
    .to_string()
}

/// `short_hold` result: the checkout session expired before submit.
pub fn checkout_expired(hold_seconds: u64) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
  <head><meta charset="utf-8"><title>Checkout expired</title></head>
  <body>
    <h1>Checkout session expired</h1>
    <p id="checkout-expired">This checkout expired after {hold_seconds} seconds. Nothing was submitted.</p>
  </body>
</html>
"#
    )
}

/// Fallback for a result page that should not happen.
pub fn internal_error(message: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
  <head><meta charset="utf-8"><title>Mock checkout error</title></head>
  <body>
    <h1>Mock checkout error</h1>
    <p id="mock-error">{message}</p>
  </body>
</html>
"#
    )
}
