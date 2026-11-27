//! Typed client for the Camofox browser REST API (`POST /tabs`,
//! `/type`, `/click`, `/evaluate`, `/navigate`, `/screenshot`; per-userId).
//!
//! Endpoint shapes were measured against a live instance in the Phase-0 spike
//! (docs/findings/camofox-spike.md, `GET /openapi.json`); the few the spike did
//! not exercise are noted at their method.
//!
//! # The rules this module keeps (ADR 0005, AGENTS.md rules 1 and 6)
//!
//! * **Card digits leave through one method only.** [`CamofoxClient::type_into`]
//!   is `pub(crate)`: the fill engine calls it, and nothing else can.
//! * **`evaluate` takes a [`DigitFreeExpression`].** It has no public
//!   constructor outside [`crate::js`], whose builders embed selectors and no
//!   values — so the fill path cannot ask for an `evaluate` fill even by
//!   mistake.
//! * **Nothing is logged and no response body is echoed.** [`CamofoxError`] is a
//!   payload-free enum; a failed request can never put a card value, or a
//!   merchant page's content, into a broker log line.
//! * **Tracing is never enabled.** The client has no tracing endpoint call at
//!   all; Playwright tracing must stay off for payment tabs because traces
//!   write fill values to disk in plaintext (spike finding A).
//! * **`/wait` is never called.** It ignores its selector and can click consent
//!   dialogs (spike finding C), which near a submit button is unacceptable.
//!
//! # Rate limits
//!
//! A stock instance allows 20 requests/minute per user. A full
//! fill→submit→settle→mask→screenshot burst is about a dozen calls over ~2 s
//! (spike Q5), so no pacing is needed here.

use std::time::Duration;

use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use secrecy::ExposeSecret;
use serde_json::{Value, json};

use crate::config::CamofoxConfig;
use crate::js::DigitFreeExpression;

/// Upper bound on any single request. Camofox has no server-side timeout on
/// `evaluate` or `navigate` (the latter destroys the session on its own 30 s
/// timeout), so the client keeps its own.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Anything the client can refuse. Deliberately payload-free: a response body
/// is a merchant page's content, and an error string is a place card data must
/// never appear (AGENTS.md rule 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CamofoxError {
    /// The request could not be sent or did not complete.
    #[error("camofox is unreachable")]
    Transport,
    /// Camofox answered with a non-success status.
    #[error("camofox refused the request")]
    Status,
    /// The response was not the shape this client understands.
    #[error("camofox returned an unusable response")]
    Response,
    /// The target URL is one `/navigate` will not accept.
    #[error("camofox will not navigate to that url")]
    Url,
    /// The page never settled inside the configured bound.
    #[error("the page did not settle")]
    SettleTimeout,
}

/// A typed Camofox REST client bound to one `userId`.
///
/// Cloning is cheap: the inner `reqwest::Client` pools connections.
#[derive(Debug, Clone)]
pub struct CamofoxClient {
    http: reqwest::Client,
    config: CamofoxConfig,
}

impl CamofoxClient {
    /// Build a client for the configured instance.
    ///
    /// # Errors
    ///
    /// [`CamofoxError::Transport`] if the HTTP client cannot be built (a TLS
    /// backend or system-configuration failure; this client is plain-HTTP).
    pub fn new(config: CamofoxConfig) -> Result<Self, CamofoxError> {
        let mut headers = HeaderMap::new();
        if let Some(key) = config.access_key() {
            let value =
                HeaderValue::from_str(key.expose_secret()).map_err(|_| CamofoxError::Response)?;
            headers.insert(AUTHORIZATION, value);
        }
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .default_headers(headers)
            .build()
            .map_err(|_| CamofoxError::Transport)?;
        Ok(Self { http, config })
    }

    /// The configuration this client was built with.
    #[must_use]
    pub fn config(&self) -> &CamofoxConfig {
        &self.config
    }

    /// `POST /tabs` — mint a tab for this `userId`, optionally pointed at `url`.
    ///
    /// The tab is created under the broker's configured `userId`, which must be
    /// the agent's (spike Q3): tabs are not transferable.
    ///
    /// # Errors
    ///
    /// [`CamofoxError`] if the request fails, is refused, or answers without a
    /// tab id.
    pub async fn create_tab(&self, url: Option<&str>) -> Result<String, CamofoxError> {
        let mut body = json!({
            "userId": self.config.user_id(),
        });
        if let Some(url) = url {
            body["url"] = json!(url);
        }
        let response = self
            .http
            .post(self.endpoint("/tabs"))
            .json(&body)
            .send()
            .await
            .map_err(|_| CamofoxError::Transport)?;
        let value = self.success_json(response).await?;
        extract_tab_id(&value).ok_or(CamofoxError::Response)
    }

    /// `POST /tabs/:id/type` — the **fill primitive** (ADR 0005 Decision 3).
    ///
    /// On camofox ≥ 1.16 the plugin-bus payload for this route is length-only
    /// (upstream #9436), so the typed value never reaches a bus subscriber. The
    /// response body is never echoed: only `{"ok":true}` is expected.
    ///
    /// `pub(crate)`: [`crate::fill`] is the only caller, which is what keeps
    /// "card digits leave the broker exactly once, through the fill path"
    /// structural rather than a convention.
    pub(crate) async fn type_into(
        &self,
        tab_id: &str,
        selector: &str,
        text: &str,
    ) -> Result<(), CamofoxError> {
        let body = json!({
            "userId": self.config.user_id(),
            "selector": selector,
            "text": text,
        });
        let response = self
            .http
            .post(self.endpoint(&format!("/tabs/{tab_id}/type")))
            .json(&body)
            .send()
            .await
            .map_err(|_| CamofoxError::Transport)?;
        self.expect_ok(response).await
    }

    /// `POST /tabs/:id/evaluate` — **digit-free operations only**.
    ///
    /// The expression is published on the plugin event bus along with its
    /// result (spike Q2), which is why the parameter type cannot carry a card
    /// value: [`DigitFreeExpression`]'s constructors are all in [`crate::js`].
    ///
    /// # Errors
    ///
    /// [`CamofoxError`] on transport failure, refusal, or an unreadable body.
    pub async fn evaluate_digit_free(
        &self,
        tab_id: &str,
        expression: &DigitFreeExpression,
    ) -> Result<Value, CamofoxError> {
        let body = json!({
            "userId": self.config.user_id(),
            "expression": expression.as_str(),
        });
        let response = self
            .http
            .post(self.endpoint(&format!("/tabs/{tab_id}/evaluate")))
            .json(&body)
            .send()
            .await
            .map_err(|_| CamofoxError::Transport)?;
        let value = self.success_json(response).await?;
        value.get("result").cloned().ok_or(CamofoxError::Response)
    }

    /// `POST /tabs/:id/click` — click one element.
    ///
    /// # Errors
    ///
    /// [`CamofoxError`] on transport failure, refusal, or an unreadable body.
    pub async fn click(&self, tab_id: &str, selector: &str) -> Result<(), CamofoxError> {
        let body = json!({
            "userId": self.config.user_id(),
            "selector": selector,
        });
        let response = self
            .http
            .post(self.endpoint(&format!("/tabs/{tab_id}/click")))
            .json(&body)
            .send()
            .await
            .map_err(|_| CamofoxError::Transport)?;
        self.expect_ok(response).await
    }

    /// `POST /tabs/:id/navigate` — go to an `http(s)` URL.
    ///
    /// `about:blank` is refused locally instead of being sent: camofox rejects
    /// it server-side (spike finding B), and containment must use
    /// [`crate::js::contain_about_blank`] through `evaluate` anyway. Refusing
    /// here keeps the one legal containment path the obvious one.
    ///
    /// # Errors
    ///
    /// [`CamofoxError::Url`] for a non-`http(s)` target, otherwise
    /// [`CamofoxError`] as for any request.
    pub async fn navigate(&self, tab_id: &str, url: &str) -> Result<(), CamofoxError> {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(CamofoxError::Url);
        }
        let body = json!({
            "userId": self.config.user_id(),
            "url": url,
        });
        let response = self
            .http
            .post(self.endpoint(&format!("/tabs/{tab_id}/navigate")))
            .json(&body)
            .send()
            .await
            .map_err(|_| CamofoxError::Transport)?;
        self.expect_ok(response).await
    }

    /// `GET /tabs/:id/screenshot` — the page as PNG bytes.
    ///
    /// Callers must have cleared the payment fields first; [`crate::fill`] only
    /// captures through the fused mask-then-capture method, so this is not the
    /// path a result screenshot takes (US-10).
    ///
    /// # Errors
    ///
    /// [`CamofoxError`] on transport failure, refusal, or an empty body.
    pub async fn screenshot(&self, tab_id: &str) -> Result<Vec<u8>, CamofoxError> {
        let response = self
            .http
            .get(self.endpoint(&format!("/tabs/{tab_id}/screenshot")))
            .query(&[("userId", self.config.user_id())])
            .send()
            .await
            .map_err(|_| CamofoxError::Transport)?;
        if !response.status().is_success() {
            return Err(CamofoxError::Status);
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|_| CamofoxError::Transport)?;
        if bytes.is_empty() {
            return Err(CamofoxError::Response);
        }
        Ok(bytes.to_vec())
    }

    /// `GET /` — is the instance up and connected to a browser?
    ///
    /// Used by deployment checks, not by the payment path.
    ///
    /// # Errors
    ///
    /// [`CamofoxError`] if the instance is unreachable or refuses the request.
    pub async fn health(&self) -> Result<(), CamofoxError> {
        let response = self
            .http
            .get(self.endpoint("/"))
            .send()
            .await
            .map_err(|_| CamofoxError::Transport)?;
        self.expect_ok(response).await
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.config.base_url())
    }

    /// Accept only a 2xx whose body is a JSON object, without echoing it.
    async fn success_json(&self, response: reqwest::Response) -> Result<Value, CamofoxError> {
        if !response.status().is_success() {
            return Err(CamofoxError::Status);
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|_| CamofoxError::Transport)?;
        serde_json::from_slice(&bytes).map_err(|_| CamofoxError::Response)
    }

    /// Accept only a 2xx. The body is dropped unread: nothing it could contain
    /// (a merchant page fragment, an echo of what was typed) may reach a caller.
    async fn expect_ok(&self, response: reqwest::Response) -> Result<(), CamofoxError> {
        if response.status().is_success() {
            Ok(())
        } else {
            Err(CamofoxError::Status)
        }
    }
}

/// Pull the tab id out of a `POST /tabs` body.
///
/// The spike's transcript shows `tabId`; a couple of camofox builds answer with
/// `tab_id`, so both are accepted rather than pinning one spelling.
fn extract_tab_id(value: &Value) -> Option<String> {
    value
        .get("tabId")
        .or_else(|| value.get("tab_id"))
        .and_then(Value::as_str)
        .filter(|tab_id| !tab_id.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::extract_tab_id;

    #[test]
    fn tab_id_is_read_from_either_spelling() {
        assert_eq!(
            extract_tab_id(&json!({"ok": true, "tabId": "abc"})),
            Some("abc".to_string())
        );
        assert_eq!(
            extract_tab_id(&json!({"ok": true, "tab_id": "def"})),
            Some("def".to_string())
        );
        assert_eq!(extract_tab_id(&json!({"ok": true})), None);
        assert_eq!(extract_tab_id(&json!({"tabId": ""})), None);
    }
}
