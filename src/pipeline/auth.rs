use crate::config::Config;
use crate::state::State;

#[cfg(feature = "auth-flow")]
use anyhow::Context as _;
#[cfg(feature = "auth-flow")]
use std::fmt::Write as _;

/// Persist *only* the rotated Monzo refresh token, leaving the rest of the
/// on-disk state (payee cache, pending reviews, `last_run`) untouched. Monzo
/// invalidates the previous refresh
/// token on every successful refresh, so the new one must reach disk before any
/// fallible work; otherwise a later failure strands it and the next run replays
/// a dead token (Monzo answers 401). Re-loads the on-disk state and copies just
/// the token onto it.
pub fn persist_rotated_token(cfg: &Config, state: &State) -> anyhow::Result<()> {
    let mut on_disk = State::load(&cfg.state_path)?;
    on_disk
        .monzo
        .refresh_token
        .clone_from(&state.monzo.refresh_token);
    on_disk.save(&cfg.state_path)
}

#[cfg(feature = "auth-flow")]
/// Generate an unguessable 128-bit OAuth `state` nonce, hex-encoded (32 chars).
fn random_state() -> anyhow::Result<String> {
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf).map_err(|e| anyhow::anyhow!("RNG failure generating state: {e}"))?;
    Ok(buf.iter().fold(String::with_capacity(32), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    }))
}

#[cfg(feature = "auth-flow")]
/// Verify the OAuth `state` echoed back on the redirect matches what we sent.
/// # Errors
/// Errors if `returned` is absent or does not equal `expected` (possible CSRF).
fn verify_state(expected: &str, returned: Option<&str>) -> anyhow::Result<()> {
    match returned {
        Some(s) if s == expected => Ok(()),
        Some(_) => anyhow::bail!("OAuth state mismatch — possible CSRF; aborting auth"),
        None => anyhow::bail!("no `state` in Monzo redirect — aborting auth"),
    }
}

#[cfg(feature = "auth-flow")]
/// One-time Monzo OAuth bootstrap. Run on a machine with a browser.
pub async fn auth_monzo(cfg: &Config) -> anyhow::Result<()> {
    let redirect = url::Url::parse(&cfg.monzo_redirect_uri)
        .context("MONZO_REDIRECT_URI is not a valid URL")?;
    let port = redirect.port().unwrap_or(8080);
    let state_param = random_state()?;

    let auth_url = format!(
        "https://auth.monzo.com/?client_id={}&redirect_uri={}&response_type=code&state={}",
        urlencoding(&cfg.monzo_client_id),
        urlencoding(cfg.monzo_redirect_uri.as_str()),
        urlencoding(&state_param),
    );

    println!("Opening Monzo authorisation in your browser...");
    println!("If it doesn't open, visit:\n{auth_url}\n");
    let _ = webbrowser::open(&auth_url);

    // One-shot blocking listener for the redirect.
    let server = tiny_http::Server::http(("127.0.0.1", port))
        .map_err(|e| anyhow::anyhow!("failed to bind {port}: {e}"))?;
    println!("Waiting for redirect on 127.0.0.1:{port} ...");

    // Bounded wait: a browser that never redirects must not hang the command
    // forever. 300s is generous for the Monzo consent screen.
    let request = server
        .recv_timeout(std::time::Duration::from_mins(5))
        .context("listener error")?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "timed out after 300s waiting for the Monzo redirect; re-run `aba auth monzo`"
            )
        })?;
    let url = format!("http://localhost{}", request.url());
    let parsed = url::Url::parse(&url).context("bad redirect URL")?;
    let code = parsed
        .query_pairs()
        .find(|(k, _)| k == "code")
        .map(|(_, v)| v.into_owned())
        .context("no `code` in redirect")?;
    let returned_state = parsed
        .query_pairs()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.into_owned());
    if let Err(e) = verify_state(&state_param, returned_state.as_deref()) {
        let _ = request.respond(tiny_http::Response::from_string(
            "Authorisation rejected: state mismatch. Return to the terminal.",
        ));
        return Err(e);
    }
    let _ = request.respond(tiny_http::Response::from_string(
        "Monzo authorisation received. You can close this tab and return to the terminal.",
    ));

    // Exchange code for tokens.
    let http = crate::http::client(std::time::Duration::from_secs(30));
    let resp = http
        .post(format!("{}/oauth2/token", cfg.monzo_base_url))
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", cfg.monzo_client_id.as_str()),
            ("client_secret", cfg.monzo_client_secret.as_str()),
            ("redirect_uri", cfg.monzo_redirect_uri.as_str()),
            ("code", code.as_str()),
        ])
        .send()
        .await?
        .error_for_status()
        .context("token exchange failed")?;

    let token: serde_json::Value = resp.json().await?;
    let refresh = token["refresh_token"]
        .as_str()
        .context("no refresh_token")?;

    println!("\n=== Monzo authorised ===");
    println!("IMPORTANT: open the Monzo app and approve access now (SCA),");
    println!("or the API will only return the last 5 minutes of transactions.\n");
    println!("Paste this into the server's state file under [monzo]:\n");
    println!("refresh_token = \"{refresh}\"");
    println!(
        "\nThen run `aba accounts` on a machine that can reach Actual to map your Monzo accounts to Actual accounts."
    );
    Ok(())
}

#[cfg(feature = "auth-flow")]
/// Minimal percent-encoding for query parameters.
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

#[cfg(all(test, feature = "auth-flow"))]
mod tests {
    use super::*;

    #[test]
    fn verify_state_accepts_exact_match() {
        assert!(verify_state("abc123", Some("abc123")).is_ok());
    }

    #[test]
    fn verify_state_rejects_mismatch() {
        assert!(verify_state("abc123", Some("nope")).is_err());
    }

    #[test]
    fn verify_state_rejects_missing() {
        assert!(verify_state("abc123", None).is_err());
    }

    #[test]
    fn random_state_is_32_hex_chars_and_varies() {
        let a = random_state().unwrap();
        let b = random_state().unwrap();
        assert_eq!(a.len(), 32, "128 bits => 32 hex chars");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "two nonces must differ");
    }
}
