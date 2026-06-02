//! Shared reqwest client builder. `aba` is an unattended one-shot cron job with
//! no external watchdog, so every HTTP client MUST bound connect + total time —
//! a stalled peer would otherwise hang the process and hold the state lock
//! indefinitely.

use std::time::Duration;

/// A client with a 10s connect timeout and the given total-request timeout.
#[must_use]
pub fn client(total: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(total)
        .build()
        // Only fails if the TLS backend can't initialise — a hard startup error.
        .expect("reqwest client builder with timeouts")
}
