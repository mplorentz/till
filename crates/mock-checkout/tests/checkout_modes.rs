//! Router-level tests for the mock checkout site (spec §9).
//!
//! These drive the axum router in-process with `tower::ServiceExt::oneshot`,
//! so there is no listening socket and no HTTP client dependency. The
//! curl-driven acceptance test lives in `script/mock-checkout-curl.sh`.
//!
//! The POST bodies here use obvious placeholder values — never a real (or
//! real-looking) PAN. The site must not read them at all; only `/stats` can
//! ever observe that a submission happened.

use std::error::Error;
use std::path::{Path, PathBuf};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use mock_checkout::{AppState, Config, router};
use serde_json::Value;
use tower::ServiceExt;

type TestResult<T> = Result<T, Box<dyn Error>>;

/// Placeholder form body. Field names match the spec `field_kinds`.
const FORM_BODY: &str = "card_number=0000-0000-0000-0000&expiry=00%2F00&cvv=000\
&cardholder_name=Placeholder%20Only&billing_postcode=0000";

fn temp_stats_path(tag: &str) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "mock-checkout-test-{}-{}-{}.jsonl",
        tag,
        std::process::id(),
        seq
    ))
}

fn app_for(stats_file: &Path) -> TestResult<Router> {
    let state = AppState::new(Config::new(stats_file))?;
    Ok(router(state))
}

async fn get(app: &Router, uri: &str) -> TestResult<(StatusCode, String)> {
    let request = Request::builder().uri(uri).body(Body::empty())?;
    collect(app.clone().oneshot(request).await?).await
}

async fn post(app: &Router, uri: &str) -> TestResult<(StatusCode, String)> {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(FORM_BODY))?;
    collect(app.clone().oneshot(request).await?).await
}

async fn collect(response: axum::response::Response) -> TestResult<(StatusCode, String)> {
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await?;
    Ok((status, String::from_utf8(bytes.to_vec())?))
}

async fn stats(app: &Router) -> TestResult<Value> {
    let (status, body) = get(app, "/stats").await?;
    assert_eq!(status, StatusCode::OK, "/stats must answer 200");
    Ok(serde_json::from_str(&body)?)
}

fn cleanup(path: &Path) {
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn checkout_form_matches_spec_field_kinds() -> TestResult<()> {
    let path = temp_stats_path("form");
    let app = app_for(&path)?;

    let (status, body) = get(&app, "/checkout?mode=success").await?;
    assert_eq!(status, StatusCode::OK);

    for field_kind in mock_checkout::pages::FIELD_KINDS {
        assert!(
            body.contains(&format!(r#"id="{field_kind}""#)),
            "form must carry an input with id={field_kind:?} for the broker's selectors"
        );
        assert!(body.contains(&format!(r#"name="{field_kind}""#)));
    }
    assert!(body.contains(r#"id="submit""#));

    // The form posts back to the mode it was opened under, with no hidden
    // state beyond the card fields themselves.
    assert!(body.contains(r#"action="/checkout?mode=success""#));

    cleanup(&path);
    Ok(())
}

#[tokio::test]
async fn success_mode_returns_order_reference() -> TestResult<()> {
    let path = temp_stats_path("success");
    let app = app_for(&path)?;

    let (status, body) = post(&app, "/checkout?mode=success").await?;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(r#"id="order-reference""#) && body.contains("MC-"),
        "success mode must show an order reference, got: {body}"
    );

    cleanup(&path);
    Ok(())
}

#[tokio::test]
async fn declined_mode_returns_card_declined_error() -> TestResult<()> {
    let path = temp_stats_path("declined");
    let app = app_for(&path)?;

    let (status, body) = post(&app, "/checkout?mode=declined").await?;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(r#"id="card-declined""#) && body.contains("declined"),
        "declined mode must show the card-declined error, got: {body}"
    );
    assert!(
        !body.contains("order-reference"),
        "declined mode must not show an order reference"
    );

    cleanup(&path);
    Ok(())
}

#[tokio::test]
async fn ambiguous_mode_returns_indefinite_processing_page() -> TestResult<()> {
    let path = temp_stats_path("ambiguous");
    let app = app_for(&path)?;

    let (status, body) = post(&app, "/checkout?mode=ambiguous").await?;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(r#"id="processing-spinner""#) && body.contains("Processing"),
        "ambiguous mode must show a processing spinner, got: {body}"
    );
    for terminal in ["order-reference", "card-declined", "checkout-expired"] {
        assert!(
            !body.contains(terminal),
            "ambiguous mode must never reach a terminal result ({terminal})"
        );
    }

    // The spinner page is all the broker will ever see: the site offers no
    // endpoint that resolves an ambiguous submission.
    let (again_status, again_body) = post(&app, "/checkout?mode=ambiguous").await?;
    assert_eq!(again_status, StatusCode::OK);
    assert!(again_body.contains(r#"id="processing-spinner""#));

    cleanup(&path);
    Ok(())
}

#[tokio::test]
async fn short_hold_submission_within_ttl_succeeds() -> TestResult<()> {
    let path = temp_stats_path("hold-in-ttl");
    let app = app_for(&path)?;

    let (open_status, form) = get(
        &app,
        "/checkout?mode=short_hold&token=inside&hold_seconds=60",
    )
    .await?;
    assert_eq!(open_status, StatusCode::OK);
    assert!(
        form.contains("token=inside") && form.contains("expires"),
        "short_hold form must carry its hold token and show the TTL, got: {form}"
    );

    let (status, body) = post(&app, "/checkout?mode=short_hold&token=inside").await?;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(r#"id="order-reference""#),
        "a short_hold submit inside the TTL behaves like success, got: {body}"
    );

    cleanup(&path);
    Ok(())
}

#[tokio::test]
async fn short_hold_submission_after_ttl_is_expired() -> TestResult<()> {
    let path = temp_stats_path("hold-expired");
    let app = app_for(&path)?;

    let (open_status, _) = get(
        &app,
        "/checkout?mode=short_hold&token=outside&hold_seconds=0",
    )
    .await?;
    assert_eq!(open_status, StatusCode::OK);

    let (status, body) = post(&app, "/checkout?mode=short_hold&token=outside").await?;
    assert_eq!(
        status,
        StatusCode::GONE,
        "a submit past the hold TTL must be rejected"
    );
    assert!(
        body.contains(r#"id="checkout-expired""#),
        "expected the checkout-expired page, got: {body}"
    );

    // A rejected submit never reached the merchant, so it is not counted.
    let snapshot = stats(&app).await?;
    assert_eq!(snapshot["total"], 0, "expired submits must not be counted");

    // Holds are single-use: replaying a valid token is also expired.
    let (replay_status, _) = post(&app, "/checkout?mode=short_hold&token=missing").await?;
    assert_eq!(replay_status, StatusCode::GONE);

    cleanup(&path);
    Ok(())
}

#[tokio::test]
async fn stats_endpoint_counts_each_submission_exactly_once() -> TestResult<()> {
    let path = temp_stats_path("stats");
    let app = app_for(&path)?;

    for mode in ["success", "success", "declined", "ambiguous"] {
        let (status, _) = post(&app, &format!("/checkout?mode={mode}")).await?;
        assert_eq!(
            status,
            StatusCode::OK,
            "submit for mode {mode} must succeed"
        );
    }

    let snapshot = stats(&app).await?;
    assert_eq!(snapshot["total"], 4, "each submit is counted exactly once");
    assert_eq!(snapshot["by_mode"]["success"], 2);
    assert_eq!(snapshot["by_mode"]["declined"], 1);
    assert_eq!(snapshot["by_mode"]["ambiguous"], 1);
    assert!(
        snapshot["by_mode"].get("short_hold").is_none(),
        "modes that were not submitted never appear in by_mode"
    );

    cleanup(&path);
    Ok(())
}

#[tokio::test]
async fn restart_persists_count_end_to_end() -> TestResult<()> {
    let path = temp_stats_path("restart");

    {
        let before_restart = app_for(&path)?;
        post(&before_restart, "/checkout?mode=success").await?;
        post(&before_restart, "/checkout?mode=declined").await?;
        assert_eq!(stats(&before_restart).await?["total"], 2);
    }

    // Simulates the site (or the broker) restarting against the same file:
    // the counter must not reset, or "clicked exactly once" is unprovable.
    let after_restart = app_for(&path)?;
    post(&after_restart, "/checkout?mode=success").await?;
    let snapshot = stats(&after_restart).await?;
    assert_eq!(snapshot["total"], 3, "counter survives a restart");
    assert_eq!(snapshot["by_mode"]["success"], 2);
    assert_eq!(snapshot["by_mode"]["declined"], 1);

    cleanup(&path);
    Ok(())
}
