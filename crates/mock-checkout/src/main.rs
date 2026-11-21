//! `cargo run -p mock-checkout` — serve the 4-mode mock checkout site.
//!
//! Environment:
//! - `MOCK_CHECKOUT_BIND`: listen address (default `127.0.0.1:9390`)
//! - `MOCK_CHECKOUT_MODE`: default mode when no `?mode=` is given
//! - `MOCK_CHECKOUT_STATS_FILE`: append-only counter file (default
//!   `mock-checkout-stats.jsonl`)
//! - `MOCK_CHECKOUT_HOLD_SECONDS`: `short_hold` TTL (default 60)

use std::error::Error;
use std::process::ExitCode;

use mock_checkout::{AppState, Config, router};

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("mock-checkout: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn Error>> {
    let config = Config::from_env()?;
    let state = AppState::new(config.clone())?;
    let app = router(state);

    let listener = tokio::net::TcpListener::bind(&config.bind_addr).await?;
    let addr = listener.local_addr()?;
    println!(
        "mock-checkout listening on http://{addr} (default mode {}, stats file {})",
        config.default_mode,
        config.stats_file.display()
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        eprintln!("mock-checkout: could not listen for ctrl-c: {error}");
    }
}
