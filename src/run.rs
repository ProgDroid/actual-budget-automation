//! Run pipeline and auth bootstrap.

use std::collections::BTreeMap;

use anyhow::Context;
use chrono::{NaiveDate, Utc};
use tracing::{info, warn};

use crate::actual::ActualClient;
use crate::actual::shim_protocol::{ImportTxn, Payee, ScheduleRow};
use crate::banks::{BankError, BankSource, RawTransaction};
use crate::claude::{ClaudeMatcher, PayeeMatch};
use crate::config::{Config, DATE_TOLERANCE_DAYS, PAYEE_THRESHOLD};
use crate::matching::{Schedule, matches_scheduled};
use crate::notify::Notifier;
use crate::state::State;

/// Result of splitting fetched transactions against the payee cache.
#[derive(Debug, PartialEq)]
pub struct ResolutionPlan {
    /// (external_id, payee_name) for cache hits.
    pub cached: Vec<(String, String)>,
    /// Distinct raw merchant names that need a Claude lookup.
    pub needs_claude: Vec<String>,
}

pub fn plan_resolution(txs: &[RawTransaction], cache: &BTreeMap<String, String>) -> ResolutionPlan {
    let mut cached = Vec::new();
    let mut needs_claude = Vec::new();
    for tx in txs {
        if let Some(payee) = cache.get(&tx.raw_merchant) {
            cached.push((tx.external_id.clone(), payee.clone()));
        } else if !needs_claude.contains(&tx.raw_merchant) {
            needs_claude.push(tx.raw_merchant.clone());
        }
    }
    ResolutionPlan {
        cached,
        needs_claude,
    }
}

/// Result of applying Claude matches to the transactions that needed lookup.
#[derive(Debug, PartialEq)]
pub struct MatchOutcome {
    /// (external_id, payee_name) successfully matched.
    pub matched: Vec<(String, String)>,
    /// Transactions with no match — to be held for review.
    pub unmatched: Vec<RawTransaction>,
}

pub fn apply_matches(txs: &[RawTransaction], matches: &[PayeeMatch]) -> MatchOutcome {
    let lookup: BTreeMap<&str, Option<&String>> = matches
        .iter()
        .map(|m| (m.raw.as_str(), m.payee.as_ref()))
        .collect();
    let mut matched = Vec::new();
    let mut unmatched = Vec::new();
    for tx in txs {
        match lookup.get(tx.raw_merchant.as_str()) {
            Some(Some(payee)) => matched.push((tx.external_id.clone(), (*payee).clone())),
            _ => unmatched.push(tx.clone()),
        }
    }
    MatchOutcome { matched, unmatched }
}

/// Build `Schedule`s for matching by resolving each row's payee id to a name.
fn schedules_from_rows(rows: Vec<ScheduleRow>, payees: &[Payee]) -> Vec<Schedule> {
    let by_id: BTreeMap<&str, &str> = payees
        .iter()
        .map(|p| (p.id.as_str(), p.name.as_str()))
        .collect();
    rows.into_iter()
        .filter_map(|r| {
            let date = NaiveDate::parse_from_str(r.date.as_str(), "%Y-%m-%d").ok()?;
            let payee = r
                .payee_id
                .as_deref()
                .and_then(|id| by_id.get(id).copied())
                .unwrap_or("")
                .to_string();
            Some(Schedule {
                amount: r.amount,
                date,
                payee,
            })
        })
        .collect()
}

/// Map a payee name to its id, for building no-auto-create import rows.
fn payee_id_for<'a>(payees: &'a [Payee], name: &str) -> Option<&'a str> {
    payees
        .iter()
        .find(|p| p.name == name)
        .map(|p| p.id.as_str())
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    cfg: &Config,
    dry_run: bool,
    state: &mut State,
    bank: &mut dyn BankSource,
    actual: &ActualClient,
    claude: &dyn ClaudeMatcher,
    notifier: &dyn Notifier,
) -> anyhow::Result<()> {
    // 1. Auth (rotated token persisted into state even in dry-run).
    match bank.ensure_auth(state).await {
        Ok(()) => {}
        Err(BankError::AuthExpired(msg)) => {
            warn!("{} auth expired: {msg}", bank.name());
            if !dry_run {
                notifier
                    .notify(
                        "Monzo re-auth required",
                        "Run `aba auth monzo` on a machine with a browser and paste the new refresh_token into the server state file.",
                    )
                    .await
                    .ok();
                state.save(&cfg.state_path)?;
            }
            return Ok(()); // clean exit; nothing else this phase
        }
        Err(BankError::Other(e)) => return Err(e),
    }

    // 2. Fetch since last_run (default: 30 days back on first ever run).
    let since = state
        .last_run
        .unwrap_or_else(|| Utc::now() - chrono::Duration::days(30));
    let fetched = bank.fetch_since(since).await.map_err(|e| match e {
        BankError::Other(e) => e,
        BankError::AuthExpired(m) => anyhow::anyhow!("auth expired mid-fetch: {m}"),
    })?;
    info!("fetched {} transactions since {since}", fetched.len());
    let fetched_count = fetched.len();

    // 3. Read Actual payees + schedules.
    let payees = actual.get_payees().await.context("get_payees")?;
    let schedule_rows = actual.get_scheduled().await.context("get_scheduled")?;
    let schedules = schedules_from_rows(schedule_rows, &payees);

    // 4. Drop scheduled matches.
    let candidates: Vec<RawTransaction> = fetched
        .into_iter()
        .filter(|tx| !matches_scheduled(tx, &schedules, DATE_TOLERANCE_DAYS, PAYEE_THRESHOLD))
        .collect();

    // 5a. Apply already-resolved pending reviews from previous runs.
    let resolved = state.take_resolved();

    // 5b. Cache split + Claude for the rest.
    let plan = plan_resolution(&candidates, &state.payee_cache);
    let claude_matches: Vec<PayeeMatch> = if plan.needs_claude.is_empty() {
        Vec::new()
    } else {
        let payee_names: Vec<String> = payees.iter().map(|p| p.name.clone()).collect();
        claude
            .match_payees(&plan.needs_claude, &payee_names)
            .await
            .context("claude match")?
    };
    let unknown_txs: Vec<RawTransaction> = candidates
        .iter()
        .filter(|tx| !state.payee_cache.contains_key(&tx.raw_merchant))
        .cloned()
        .collect();
    let outcome = apply_matches(&unknown_txs, &claude_matches);

    // 6. Assemble import rows (payee NAME -> id; never auto-create on the matched path).
    let mut import_rows: Vec<ImportTxn> = Vec::new();
    let mut held = 0usize;

    let push_row = |rows: &mut Vec<ImportTxn>, tx: &RawTransaction, payee_id: &str| {
        rows.push(ImportTxn {
            date: tx.date.format("%Y-%m-%d").to_string(),
            amount: tx.amount_minor,
            payee: payee_id.to_string(),
            imported_id: tx.external_id.clone(),
        });
    };

    let by_id: BTreeMap<&str, &RawTransaction> = candidates
        .iter()
        .map(|t| (t.external_id.as_str(), t))
        .collect();

    // cached hits
    for (ext_id, payee_name) in &plan.cached {
        match (
            by_id.get(ext_id.as_str()),
            payee_id_for(&payees, payee_name),
        ) {
            (Some(tx), Some(pid)) => push_row(&mut import_rows, tx, pid),
            (Some(_), None) => warn!(
                "cached payee '{payee_name}' for tx {ext_id} no longer exists in Actual; skipping (re-resolve it)"
            ),
            _ => {}
        }
    }
    // claude matches
    for (ext_id, payee_name) in &outcome.matched {
        match (
            by_id.get(ext_id.as_str()),
            payee_id_for(&payees, payee_name),
        ) {
            (Some(tx), Some(pid)) => push_row(&mut import_rows, tx, pid),
            (Some(_), None) => warn!(
                "matched payee '{payee_name}' for tx {ext_id} not found in Actual payee list; skipping"
            ),
            _ => {}
        }
    }
    // resolved pending reviews (may need create_new)
    for r in &resolved {
        let pid = match payee_id_for(&payees, &r.payee_name) {
            Some(pid) => pid.to_string(),
            None if r.create_new && !dry_run => actual.create_payee(r.payee_name.clone()).await?.id,
            None => {
                warn!(
                    "payee '{}' not found and not creating (dry_run or create_new=false)",
                    r.payee_name
                );
                continue;
            }
        };
        import_rows.push(ImportTxn {
            date: r.date.clone(),
            amount: r.amount,
            payee: pid,
            imported_id: format!("{}:{}", r.raw_merchant, r.date),
        });
    }

    // unmatched -> hold for review
    for tx in &outcome.unmatched {
        let date = tx.date.format("%Y-%m-%d").to_string();
        if state.add_pending(&tx.raw_merchant, tx.amount_minor, &date) {
            held += 1;
        }
    }

    // 7. Import or report.
    if dry_run {
        println!("DRY RUN — no writes to Actual");
        println!("  would import : {}", import_rows.len());
        println!(
            "  would skip   : {} (matched a schedule)",
            fetched_count.saturating_sub(candidates.len())
        );
        println!("  would hold   : {}", outcome.unmatched.len());
        for r in &import_rows {
            println!("    + {} {} -> payee {}", r.date, r.amount, r.payee);
        }
        for tx in &outcome.unmatched {
            println!("    ? hold {} {}", tx.raw_merchant, tx.amount_minor);
        }
        // Persist ONLY the rotated Monzo token; discard in-memory cache/pending/last_run
        // changes by re-loading the on-disk state and copying just the token onto it.
        let mut on_disk = State::load(&cfg.state_path)?;
        on_disk.monzo.refresh_token = state.monzo.refresh_token.clone();
        if on_disk.monzo.account_id.is_empty() {
            on_disk.monzo.account_id = state.monzo.account_id.clone();
        }
        on_disk.save(&cfg.state_path)?;
        return Ok(());
    }

    let summary = if import_rows.is_empty() {
        crate::actual::ImportSummary {
            added: 0,
            updated: 0,
        }
    } else {
        actual
            .import_transactions(state.monzo.account_id.clone(), import_rows)
            .await
            .context("import_transactions")?
    };
    info!(
        "imported added={} updated={} held={held}",
        summary.added, summary.updated
    );

    state.last_run = Some(Utc::now());
    state.save(&cfg.state_path)?;

    notifier
        .notify(
            "Actual import complete",
            &format!(
                "Added {}, updated {}, held {} for review.",
                summary.added, summary.updated, held
            ),
        )
        .await
        .ok();

    Ok(())
}

/// One-time Monzo OAuth bootstrap. Run on a machine with a browser.
pub async fn auth_monzo(cfg: &Config) -> anyhow::Result<()> {
    let redirect = url::Url::parse(&cfg.monzo_redirect_uri)
        .context("MONZO_REDIRECT_URI is not a valid URL")?;
    let port = redirect.port().unwrap_or(8080);
    let state_param = format!("aba{}", Utc::now().timestamp());

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

    let request = server.recv().context("listener error")?;
    let url = format!("http://localhost{}", request.url());
    let parsed = url::Url::parse(&url).context("bad redirect URL")?;
    let code = parsed
        .query_pairs()
        .find(|(k, _)| k == "code")
        .map(|(_, v)| v.into_owned())
        .context("no `code` in redirect")?;
    let _ = request.respond(tiny_http::Response::from_string(
        "Monzo authorisation received. You can close this tab and return to the terminal.",
    ));

    // Exchange code for tokens.
    let http = reqwest::Client::new();
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
    println!("\nAlso set [monzo].account_id to the Monzo account id you want to import.");
    Ok(())
}

/// Minimal percent-encoding for query parameters.
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::banks::RawTransaction;
    use crate::claude::PayeeMatch;
    use chrono::NaiveDate;
    use std::collections::BTreeMap;

    fn tx(id: &str, merchant: &str, amount: i64) -> RawTransaction {
        RawTransaction {
            external_id: id.into(),
            date: NaiveDate::from_ymd_opt(2026, 5, 30).unwrap(),
            amount_minor: amount,
            raw_merchant: merchant.into(),
        }
    }

    #[test]
    fn splits_cache_hits_unknowns_for_claude() {
        let mut cache = BTreeMap::new();
        cache.insert(
            "TFL TRAVEL CH".to_string(),
            "Transport for London".to_string(),
        );
        let txs = vec![
            tx("1", "TFL TRAVEL CH", -275),
            tx("2", "AMZNMKTPLACE", -1999),
        ];

        let plan = plan_resolution(&txs, &cache);
        assert_eq!(
            plan.cached,
            vec![("1".to_string(), "Transport for London".to_string())]
        );
        assert_eq!(plan.needs_claude, vec!["AMZNMKTPLACE".to_string()]);
    }

    #[test]
    fn applies_claude_matches_and_flags_unmatched() {
        let txs = vec![tx("2", "AMZNMKTPLACE", -1999), tx("3", "WEIRD XYZ", -100)];
        let matches = vec![
            PayeeMatch {
                raw: "AMZNMKTPLACE".into(),
                payee: Some("Amazon".into()),
            },
            PayeeMatch {
                raw: "WEIRD XYZ".into(),
                payee: None,
            },
        ];
        let outcome = apply_matches(&txs, &matches);
        assert_eq!(
            outcome.matched,
            vec![("2".to_string(), "Amazon".to_string())]
        );
        assert_eq!(outcome.unmatched.len(), 1);
        assert_eq!(outcome.unmatched[0].external_id, "3");
    }
}
