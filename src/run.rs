//! Run pipeline and auth bootstrap.

use std::collections::BTreeMap;

use anyhow::Context;
use chrono::{NaiveDate, Utc};
use tracing::{info, warn};

use crate::actual::ActualClient;
use crate::actual::shim_protocol::{ImportTxn, Payee, ScheduleRow};
use crate::banks::{BankError, BankSource, RawTransaction};
use crate::claude::{ClaudeMatcher, Confidence, PayeeMatch};
use crate::config::{Config, DATE_TOLERANCE_DAYS, PAYEE_THRESHOLD};
use crate::matching::{Schedule, best_payee_match, matches_scheduled};
use crate::notify::Notifier;
use crate::state::{PendingReview, State};
use crate::telegram::{ConfirmChannel, Prompt};

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

/// Result of applying Claude matches, split by the auto-import cutoff.
#[derive(Debug, PartialEq)]
pub struct MatchOutcome {
    /// (external_id, payee_name) confident enough to import + cache now.
    pub matched: Vec<(String, String)>,
    /// (tx, suggested_payee, confidence) held for Telegram confirmation.
    pub proposed: Vec<(RawTransaction, String, Confidence)>,
    /// No usable match — held with no suggestion.
    pub unmatched: Vec<RawTransaction>,
}

pub fn apply_matches(
    txs: &[RawTransaction],
    matches: &[PayeeMatch],
    autoimport: Confidence,
) -> MatchOutcome {
    let lookup: BTreeMap<&str, &PayeeMatch> = matches.iter().map(|m| (m.raw.as_str(), m)).collect();
    let mut matched = Vec::new();
    let mut proposed = Vec::new();
    let mut unmatched = Vec::new();
    for tx in txs {
        match lookup.get(tx.raw_merchant.as_str()) {
            Some(m) => match &m.payee {
                Some(payee) if m.confidence.rank() >= autoimport.rank() => {
                    matched.push((tx.external_id.clone(), payee.clone()))
                }
                Some(payee) => proposed.push((tx.clone(), payee.clone(), m.confidence)),
                None => unmatched.push(tx.clone()),
            },
            None => unmatched.push(tx.clone()),
        }
    }
    MatchOutcome {
        matched,
        proposed,
        unmatched,
    }
}

/// Build `Schedule`s for matching by resolving each row's payee id to a name.
fn schedules_from_rows(rows: Vec<ScheduleRow>, payees: &[Payee]) -> Vec<Schedule> {
    let by_id: BTreeMap<&str, &str> = payees
        .iter()
        .map(|p| (p.id.as_str(), p.name.as_str()))
        .collect();
    rows.into_iter()
        .filter_map(|r| {
            let date = NaiveDate::parse_from_str(r.date.as_deref()?, "%Y-%m-%d").ok()?;
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

/// Build the Telegram prompt for a held entry. Flavor is chosen by whether a
/// suggestion exists and whether it is a real payee:
/// - suggestion in payee list  -> "raw → suggestion?"  [Confirm][Create new][Skip]
/// - suggestion NOT in payees   -> "No payee 'x'. Create it?" [Create][Skip]
/// - no suggestion              -> "New merchant raw — which payee?" [Create new][Skip]
fn build_prompt(entry: &PendingReview, payee_names: &[String]) -> Prompt {
    use crate::telegram::encode_callback;
    let key = &entry.external_id;
    match &entry.suggestion {
        Some(s) if payee_names.iter().any(|p| p == s) => {
            let conf = entry.confidence.as_deref().unwrap_or("medium");
            Prompt {
                chat_text: format!(
                    "`{}` → *{}*?  _({} confidence)_\nReply with a different payee to remap.",
                    entry.raw_merchant, s, conf
                ),
                buttons: vec![
                    ("✅ Confirm".into(), encode_callback("cf", key)),
                    ("🆕 Create new".into(), encode_callback("nw", key)),
                    ("❌ Skip".into(), encode_callback("sk", key)),
                ],
            }
        }
        Some(s) => Prompt {
            chat_text: format!(
                "No existing payee *{}*. Create it?\nOr reply with a payee name.",
                s
            ),
            buttons: vec![
                ("🆕 Create".into(), encode_callback("nw", key)),
                ("❌ Skip".into(), encode_callback("sk", key)),
            ],
        },
        None => Prompt {
            chat_text: format!(
                "New merchant `{}` — which payee?\nReply with a payee name, or:",
                entry.raw_merchant
            ),
            buttons: vec![
                ("🆕 Create new".into(), encode_callback("nw", key)),
                ("❌ Skip".into(), encode_callback("sk", key)),
            ],
        },
    }
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
    channel: Option<&dyn ConfirmChannel>,
    autoimport: Confidence,
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

    // Build payee name list once (reused for remap normalisation and Claude call).
    let payee_names: Vec<String> = payees.iter().map(|p| p.name.clone()).collect();

    // Normalise any free-text remap replies against the live payee list, so
    // `take_resolved` only ever folds a real existing payee into the cache.
    // A remap that doesn't match becomes a "create it?" hold (suggestion set,
    // resolve_to cleared, prompt re-sent next run).
    for pr in &mut state.pending_reviews {
        if !pr.create_new && !pr.resolve_to.is_empty() {
            match best_payee_match(&pr.resolve_to, &payee_names, PAYEE_THRESHOLD) {
                Some(canon) => pr.resolve_to = canon,
                None => {
                    pr.suggestion = Some(std::mem::take(&mut pr.resolve_to));
                    pr.telegram_message_id = None;
                }
            }
        }
    }

    // 5a. Apply already-resolved pending reviews from previous runs.
    let resolved = state.take_resolved();

    // 5b. Cache split + Claude for the rest.
    let plan = plan_resolution(&candidates, &state.payee_cache);
    let claude_matches: Vec<PayeeMatch> = if plan.needs_claude.is_empty() {
        Vec::new()
    } else {
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
    let outcome = apply_matches(&unknown_txs, &claude_matches, autoimport);

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
    let mut newly_cached: Vec<(String, String)> = Vec::new();
    for (ext_id, payee_name) in &outcome.matched {
        match (
            by_id.get(ext_id.as_str()),
            payee_id_for(&payees, payee_name),
        ) {
            (Some(tx), Some(pid)) => {
                push_row(&mut import_rows, tx, pid);
                if !state.payee_cache.contains_key(&tx.raw_merchant) {
                    state
                        .payee_cache
                        .insert(tx.raw_merchant.clone(), payee_name.clone());
                    newly_cached.push((tx.raw_merchant.clone(), payee_name.clone()));
                }
            }
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
            imported_id: if r.external_id.is_empty() {
                format!("{}:{}", r.raw_merchant, r.date)
            } else {
                r.external_id.clone()
            },
        });
    }

    // proposed (medium/low) -> held WITH suggestion + confidence, NOT imported
    for (tx, sugg, conf) in &outcome.proposed {
        let date = tx.date.format("%Y-%m-%d").to_string();
        if state.add_pending_full(
            &tx.external_id,
            &tx.raw_merchant,
            tx.amount_minor,
            &date,
            Some(sugg.clone()),
            Some(conf.as_str().to_string()),
        ) {
            held += 1;
        }
    }
    // unmatched -> held with no suggestion
    for tx in &outcome.unmatched {
        let date = tx.date.format("%Y-%m-%d").to_string();
        if state.add_pending(&tx.external_id, &tx.raw_merchant, tx.amount_minor, &date) {
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
        println!("  would hold   : {held}");
        for r in &import_rows {
            println!("    + {} {} -> payee {}", r.date, r.amount, r.payee);
        }
        for (tx, sugg, _) in &outcome.proposed {
            println!(
                "    ? hold {} {} (suggest {})",
                tx.raw_merchant, tx.amount_minor, sugg
            );
        }
        for tx in &outcome.unmatched {
            println!("    ? hold {} {}", tx.raw_merchant, tx.amount_minor);
        }
        if !newly_cached.is_empty() {
            println!("  would cache  : {}", newly_cached.len());
            for (raw, payee) in &newly_cached {
                println!("    ~ {raw} -> {payee}");
            }
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

    // Send a prompt for every held entry that hasn't been prompted yet.
    if let Some(ch) = channel {
        // collect indices to avoid borrow conflicts while sending
        let to_prompt: Vec<usize> = state
            .pending_reviews
            .iter()
            .enumerate()
            .filter(|(_, p)| p.telegram_message_id.is_none())
            .map(|(i, _)| i)
            .collect();
        for i in to_prompt {
            let prompt = build_prompt(&state.pending_reviews[i], &payee_names);
            match ch.send_prompt(&prompt).await {
                Ok(message_id) => state.pending_reviews[i].telegram_message_id = Some(message_id),
                Err(e) => warn!("telegram send_prompt failed (will retry next run): {e}"),
            }
        }
    }

    state.save(&cfg.state_path)?;

    let awaiting = state.pending_reviews.len();
    let mut body = format!(
        "Added {}, updated {}, held {} for review.",
        summary.added, summary.updated, held
    );
    if !newly_cached.is_empty() {
        body.push_str(&format!("\nNewly auto-matched ({}):", newly_cached.len()));
        for (raw, payee) in &newly_cached {
            body.push_str(&format!("\n  {raw} -> {payee}"));
        }
    }
    if awaiting > 0 {
        body.push_str(&format!("\n{awaiting} item(s) awaiting your confirmation."));
    }
    notifier.notify("Actual import complete", &body).await.ok();

    Ok(())
}

/// Generate an unguessable 128-bit OAuth `state` nonce, hex-encoded (32 chars).
fn random_state() -> anyhow::Result<String> {
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf).map_err(|e| anyhow::anyhow!("RNG failure generating state: {e}"))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

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

    let request = server.recv().context("listener error")?;
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
pub(crate) mod tests {
    use super::*;
    use crate::actual::ActualClient;
    use crate::banks::{BankError, BankSource, RawTransaction};
    use crate::claude::{ClaudeMatcher, Confidence, PayeeMatch};
    use crate::notify::Notifier;
    use crate::state::PendingReview;
    use crate::telegram::{ConfirmChannel, Prompt, Reply};
    use async_trait::async_trait;
    use chrono::{DateTime, NaiveDate, Utc};
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use tempfile::TempDir;

    pub fn tx(id: &str, merchant: &str, amount: i64) -> RawTransaction {
        RawTransaction {
            external_id: id.into(),
            date: NaiveDate::from_ymd_opt(2026, 5, 30).unwrap(),
            amount_minor: amount,
            raw_merchant: merchant.into(),
        }
    }

    /// Test double: returns scripted replies once, records sent prompts.
    pub struct FakeChannel {
        pub scripted: Mutex<Vec<Reply>>,
        pub offset: i64,
        pub sent: Mutex<Vec<Prompt>>,
        pub next_id: Mutex<i64>,
    }
    impl FakeChannel {
        pub fn new(scripted: Vec<Reply>, offset: i64) -> Self {
            Self {
                scripted: Mutex::new(scripted),
                offset,
                sent: Mutex::new(Vec::new()),
                next_id: Mutex::new(1000),
            }
        }
    }
    #[async_trait]
    impl ConfirmChannel for FakeChannel {
        async fn poll_replies(
            &self,
            _since: i64,
            _msg_map: &BTreeMap<i64, String>,
        ) -> anyhow::Result<(Vec<Reply>, i64)> {
            Ok((
                std::mem::take(&mut *self.scripted.lock().unwrap()),
                self.offset,
            ))
        }
        async fn send_prompt(&self, prompt: &Prompt) -> anyhow::Result<i64> {
            self.sent.lock().unwrap().push(prompt.clone());
            let mut id = self.next_id.lock().unwrap();
            *id += 1;
            Ok(*id)
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
    fn apply_matches_partitions_by_confidence() {
        let txs = vec![
            tx("1", "TESCO", -500),     // high -> auto import
            tx("2", "SQ *CAFE", -450),  // medium -> proposed/held
            tx("3", "WEIRD XYZ", -100), // none -> unmatched/held
        ];
        let matches = vec![
            PayeeMatch {
                raw: "TESCO".into(),
                payee: Some("Tesco".into()),
                confidence: Confidence::High,
            },
            PayeeMatch {
                raw: "SQ *CAFE".into(),
                payee: Some("Local Cafe".into()),
                confidence: Confidence::Medium,
            },
            PayeeMatch {
                raw: "WEIRD XYZ".into(),
                payee: None,
                confidence: Confidence::Low,
            },
        ];
        let outcome = apply_matches(&txs, &matches, Confidence::High);
        assert_eq!(
            outcome.matched,
            vec![("1".to_string(), "Tesco".to_string())]
        );
        assert_eq!(outcome.proposed.len(), 1);
        assert_eq!(outcome.proposed[0].0.external_id, "2");
        assert_eq!(outcome.proposed[0].1, "Local Cafe");
        assert_eq!(outcome.proposed[0].2, Confidence::Medium);
        assert_eq!(outcome.unmatched.len(), 1);
        assert_eq!(outcome.unmatched[0].external_id, "3");
    }

    #[test]
    fn build_prompt_picks_flavor_by_payee_membership() {
        let payees = vec!["Tesco".to_string(), "Local Cafe".to_string()];

        // proposed, suggestion exists -> confirm/create/skip
        let proposed = PendingReview {
            external_id: "tx1".into(),
            raw_merchant: "SQ *CAFE".into(),
            amount: -450,
            date: "2026-06-01".into(),
            resolve_to: String::new(),
            create_new: false,
            suggestion: Some("Local Cafe".into()),
            confidence: Some("medium".into()),
            telegram_message_id: None,
        };
        let p = build_prompt(&proposed, &payees);
        assert!(p.chat_text.contains("Local Cafe"));
        assert_eq!(p.buttons.len(), 3);
        assert_eq!(p.buttons[0].1, "cf:tx1");

        // no-match -> create/skip only
        let unmatched = PendingReview {
            external_id: "tx2".into(),
            raw_merchant: "MYSTERY".into(),
            amount: -10,
            date: "2026-06-01".into(),
            resolve_to: String::new(),
            create_new: false,
            suggestion: None,
            confidence: None,
            telegram_message_id: None,
        };
        let p2 = build_prompt(&unmatched, &payees);
        assert_eq!(p2.buttons.len(), 2);
        assert_eq!(p2.buttons[0].1, "nw:tx2");

        // failed remap: suggestion set but NOT in payees -> "create it?" flavor
        let failed = PendingReview {
            external_id: "tx3".into(),
            raw_merchant: "X".into(),
            amount: -10,
            date: "2026-06-01".into(),
            resolve_to: String::new(),
            create_new: false,
            suggestion: Some("Sainsbury's".into()),
            confidence: None,
            telegram_message_id: None,
        };
        let p3 = build_prompt(&failed, &payees);
        assert!(p3.chat_text.contains("Sainsbury's"));
        assert_eq!(p3.buttons.len(), 2);
        assert_eq!(p3.buttons[0].1, "nw:tx3"); // create
        assert_eq!(p3.buttons[1].1, "sk:tx3"); // skip
    }

    // ---- Task 10: in-process doubles ----

    struct FakeBank {
        txs: Vec<RawTransaction>,
    }
    #[async_trait]
    impl BankSource for FakeBank {
        fn name(&self) -> &'static str {
            "fake"
        }
        async fn ensure_auth(&mut self, _s: &mut State) -> Result<(), BankError> {
            Ok(())
        }
        async fn fetch_since(
            &self,
            _since: DateTime<Utc>,
        ) -> Result<Vec<RawTransaction>, BankError> {
            Ok(self.txs.clone())
        }
    }

    struct FakeClaude {
        out: Vec<PayeeMatch>,
    }
    #[async_trait]
    impl ClaudeMatcher for FakeClaude {
        async fn match_payees(
            &self,
            _raw: &[String],
            _p: &[String],
        ) -> anyhow::Result<Vec<PayeeMatch>> {
            Ok(self.out.clone())
        }
    }

    struct NullNotifier;
    #[async_trait]
    impl Notifier for NullNotifier {
        async fn notify(&self, _t: &str, _b: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    /// Notifier double that records every notify(title, body) call.
    struct RecordingNotifier {
        calls: Mutex<Vec<(String, String)>>,
    }
    impl RecordingNotifier {
        fn new() -> Self {
            Self { calls: Mutex::new(Vec::new()) }
        }
    }
    #[async_trait]
    impl Notifier for RecordingNotifier {
        async fn notify(&self, t: &str, b: &str) -> anyhow::Result<()> {
            self.calls.lock().unwrap().push((t.to_string(), b.to_string()));
            Ok(())
        }
    }

    /// Bank whose `ensure_auth` always reports the 90-day re-auth condition.
    struct AuthExpiredBank;
    #[async_trait]
    impl BankSource for AuthExpiredBank {
        fn name(&self) -> &'static str {
            "fake"
        }
        async fn ensure_auth(&mut self, _s: &mut State) -> Result<(), BankError> {
            Err(BankError::AuthExpired("test".into()))
        }
        async fn fetch_since(
            &self,
            _since: DateTime<Utc>,
        ) -> Result<Vec<RawTransaction>, BankError> {
            panic!("fetch_since must not be called after AuthExpired");
        }
    }

    /// Bank that simulates Monzo's refresh-token rotation during `ensure_auth`.
    struct RotatingBank {
        rotate_to: String,
    }
    #[async_trait]
    impl BankSource for RotatingBank {
        fn name(&self) -> &'static str {
            "fake"
        }
        async fn ensure_auth(&mut self, s: &mut State) -> Result<(), BankError> {
            s.monzo.refresh_token = self.rotate_to.clone();
            Ok(())
        }
        async fn fetch_since(
            &self,
            _since: DateTime<Utc>,
        ) -> Result<Vec<RawTransaction>, BankError> {
            Ok(vec![])
        }
    }

    /// Build an `ActualClient` backed by a fake node shim in `dir`.
    /// `get_payees` returns `[{p_cafe, Local Cafe}]`, `get_scheduled` is empty,
    /// `create_payee` returns `{id: p_new}`, and `import_transactions` appends
    /// the imported rows to `<dir>/imports.log` (one JSON array per call).
    fn fake_actual(dir: &TempDir) -> ActualClient {
        let script = r#"
let b='';process.stdin.on('data',c=>b+=c);process.stdin.on('end',()=>{
  const req=JSON.parse(b); const out=o=>process.stdout.write(JSON.stringify(o));
  const fs=require('fs'), path=require('path');
  if(req.op==='get_payees'){ out({ok:true,data:[{id:'p_cafe',name:'Local Cafe'}]}); }
  else if(req.op==='get_scheduled'){ out({ok:true,data:[]}); }
  else if(req.op==='create_payee'){ out({ok:true,data:{id:'p_new'}}); }
  else if(req.op==='import_transactions'){
    fs.appendFileSync(path.join(__dirname,'imports.log'), JSON.stringify(req.transactions)+'\n');
    out({ok:true,data:{added:req.transactions.length,updated:0}});
  } else { out({ok:false,error:'unknown op'}); }
});
"#;
        std::fs::write(dir.path().join("index.js"), script).unwrap();
        ActualClient::new(
            dir.path().to_path_buf(),
            false,
            "http://t".into(),
            "pw".into(),
            "sync".into(),
        )
    }

    /// Like `fake_actual` but `import_transactions` fails, and `create_payee`
    /// records that it was called (to `payees.log`) before returning an id.
    fn fake_actual_failing_import(dir: &TempDir) -> ActualClient {
        let script = r#"
let b='';process.stdin.on('data',c=>b+=c);process.stdin.on('end',()=>{
  const req=JSON.parse(b); const out=o=>process.stdout.write(JSON.stringify(o));
  const fs=require('fs'), path=require('path');
  if(req.op==='get_payees'){ out({ok:true,data:[{id:'p_cafe',name:'Local Cafe'}]}); }
  else if(req.op==='get_scheduled'){ out({ok:true,data:[]}); }
  else if(req.op==='create_payee'){
    fs.appendFileSync(path.join(__dirname,'payees.log'), 'called\n');
    out({ok:true,data:{id:'p_new'}});
  }
  else if(req.op==='import_transactions'){ out({ok:false,error:'boom'}); }
  else { out({ok:false,error:'unknown op'}); }
});
"#;
        std::fs::write(dir.path().join("index.js"), script).unwrap();
        ActualClient::new(
            dir.path().to_path_buf(),
            false,
            "http://t".into(),
            "pw".into(),
            "sync".into(),
        )
    }

    fn cfg_at(state_path: &std::path::Path) -> Config {
        Config::from_getter(|k| match k {
            "ANTHROPIC_API_KEY" => Some("ak".into()),
            "MONZO_CLIENT_ID" => Some("c".into()),
            "MONZO_CLIENT_SECRET" => Some("s".into()),
            "MONZO_REDIRECT_URI" => Some("http://localhost/cb".into()),
            "APPRISE_URL" => Some("http://a/notify".into()),
            "ACTUAL_SERVER_URL" => Some("http://actual".into()),
            "ACTUAL_PASSWORD" => Some("pw".into()),
            "ACTUAL_SYNC_ID" => Some("sync".into()),
            "ABA_STATE_PATH" => Some(state_path.to_string_lossy().to_string()),
            _ => None,
        })
        .unwrap()
    }

    // ---- Task 10: integration tests ----

    #[tokio::test]
    async fn medium_match_held_prompted_then_imported_after_confirm() {
        let shim = tempfile::tempdir().unwrap();
        let statedir = tempfile::tempdir().unwrap();
        let state_path = statedir.path().join("s.toml");
        let imports_log = shim.path().join("imports.log");
        let cfg = cfg_at(&state_path);
        let actual = fake_actual(&shim);
        let notifier = NullNotifier;
        let channel = FakeChannel::new(vec![], 0);

        // ---- Run #1: one medium-confidence match -> held + prompted, NOT imported.
        let mut state = State::default();
        let mut bank = FakeBank {
            txs: vec![tx("tx_cafe", "SQ *CAFE", -450)],
        };
        let claude = FakeClaude {
            out: vec![PayeeMatch {
                raw: "SQ *CAFE".into(),
                payee: Some("Local Cafe".into()),
                confidence: Confidence::Medium,
            }],
        };
        run(
            &cfg,
            false,
            &mut state,
            &mut bank,
            &actual,
            &claude,
            &notifier,
            Some(&channel as &dyn ConfirmChannel),
            Confidence::High,
        )
        .await
        .unwrap();

        assert!(!imports_log.exists(), "nothing should be imported yet");
        assert_eq!(state.pending_reviews.len(), 1);
        assert_eq!(
            state.pending_reviews[0].suggestion.as_deref(),
            Some("Local Cafe")
        );
        assert!(state.pending_reviews[0].telegram_message_id.is_some());
        assert_eq!(channel.sent.lock().unwrap().len(), 1);

        // ---- User confirms (what `poll` would do).
        assert!(state.confirm_pending("tx_cafe"));

        // ---- Run #2: no new txs -> the held tx is now imported, prompt NOT re-sent.
        let mut bank2 = FakeBank { txs: vec![] };
        let claude2 = FakeClaude { out: vec![] };
        run(
            &cfg,
            false,
            &mut state,
            &mut bank2,
            &actual,
            &claude2,
            &notifier,
            Some(&channel as &dyn ConfirmChannel),
            Confidence::High,
        )
        .await
        .unwrap();

        let logged = std::fs::read_to_string(&imports_log).unwrap();
        assert!(
            logged.contains("tx_cafe"),
            "held tx imported under imported_id"
        );
        assert!(
            logged.contains("p_cafe"),
            "imported under the Local Cafe payee id"
        );
        assert!(state.pending_reviews.is_empty(), "resolved entry removed");
        assert_eq!(
            channel.sent.lock().unwrap().len(),
            1,
            "no re-prompt (idempotent)"
        );
    }

    #[tokio::test]
    async fn dry_run_sends_no_prompts_and_no_writes() {
        let shim = tempfile::tempdir().unwrap();
        let statedir = tempfile::tempdir().unwrap();
        let state_path = statedir.path().join("s.toml");
        let imports_log = shim.path().join("imports.log");
        let cfg = cfg_at(&state_path);
        let actual = fake_actual(&shim);
        let notifier = NullNotifier;
        let channel = FakeChannel::new(vec![], 0);

        let mut state = State::default();
        let mut bank = FakeBank {
            txs: vec![tx("tx_cafe", "SQ *CAFE", -450)],
        };
        let claude = FakeClaude {
            out: vec![PayeeMatch {
                raw: "SQ *CAFE".into(),
                payee: Some("Local Cafe".into()),
                confidence: Confidence::Medium,
            }],
        };
        run(
            &cfg,
            true,
            &mut state,
            &mut bank,
            &actual,
            &claude,
            &notifier,
            Some(&channel as &dyn ConfirmChannel),
            Confidence::High,
        )
        .await
        .unwrap();

        assert!(
            channel.sent.lock().unwrap().is_empty(),
            "dry-run sends no prompts"
        );
        assert!(!imports_log.exists(), "dry-run writes nothing to Actual");
        let on_disk = State::load(&state_path).unwrap();
        assert!(
            on_disk.pending_reviews.is_empty(),
            "dry-run does not persist holds"
        );
        assert_eq!(on_disk.last_run, None, "dry-run does not advance last_run");
    }

    #[tokio::test]
    async fn unconfirmed_hold_is_not_reprompted() {
        let shim = tempfile::tempdir().unwrap();
        let statedir = tempfile::tempdir().unwrap();
        let state_path = statedir.path().join("s.toml");
        let cfg = cfg_at(&state_path);
        let actual = fake_actual(&shim);
        let notifier = NullNotifier;
        let channel = FakeChannel::new(vec![], 0);

        // ---- Run #1: one medium-confidence match -> held + prompted.
        let mut state = State::default();
        let mut bank = FakeBank {
            txs: vec![tx("tx_cafe", "SQ *CAFE", -450)],
        };
        let claude = FakeClaude {
            out: vec![PayeeMatch {
                raw: "SQ *CAFE".into(),
                payee: Some("Local Cafe".into()),
                confidence: Confidence::Medium,
            }],
        };
        run(
            &cfg,
            false,
            &mut state,
            &mut bank,
            &actual,
            &claude,
            &notifier,
            Some(&channel as &dyn ConfirmChannel),
            Confidence::High,
        )
        .await
        .unwrap();

        assert_eq!(state.pending_reviews.len(), 1);
        assert!(state.pending_reviews[0].telegram_message_id.is_some());
        assert_eq!(channel.sent.lock().unwrap().len(), 1);

        // ---- User does NOT confirm — the hold stays pending/unconfirmed.

        // ---- Run #2: no new txs -> the already-prompted hold is NOT re-prompted.
        let mut bank2 = FakeBank { txs: vec![] };
        let claude2 = FakeClaude { out: vec![] };
        run(
            &cfg,
            false,
            &mut state,
            &mut bank2,
            &actual,
            &claude2,
            &notifier,
            Some(&channel as &dyn ConfirmChannel),
            Confidence::High,
        )
        .await
        .unwrap();

        assert_eq!(state.pending_reviews.len(), 1, "still held (not confirmed)");
        assert_eq!(
            channel.sent.lock().unwrap().len(),
            1,
            "telegram_message_id.is_none() filter skipped the already-prompted entry"
        );
    }

    #[tokio::test]
    async fn auth_expired_notifies_and_saves_when_not_dry_run() {
        let shim = tempfile::tempdir().unwrap();
        let statedir = tempfile::tempdir().unwrap();
        let state_path = statedir.path().join("s.toml");
        let imports_log = shim.path().join("imports.log");
        let cfg = cfg_at(&state_path);
        let actual = fake_actual(&shim);
        let notifier = RecordingNotifier::new();

        let mut state = State::default();
        let mut bank = AuthExpiredBank;
        let claude = FakeClaude { out: vec![] };
        run(
            &cfg, false, &mut state, &mut bank, &actual, &claude, &notifier, None, Confidence::High,
        )
        .await
        .unwrap();

        let calls = notifier.calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "one re-auth notification");
        assert_eq!(calls[0].0, "Monzo re-auth required");
        assert!(state_path.exists(), "state saved on !dry_run auth-expired");
        assert!(!imports_log.exists(), "no fetch/import after auth-expired");
    }

    #[tokio::test]
    async fn auth_expired_dry_run_does_not_notify_or_save() {
        let shim = tempfile::tempdir().unwrap();
        let statedir = tempfile::tempdir().unwrap();
        let state_path = statedir.path().join("s.toml");
        let cfg = cfg_at(&state_path);
        let actual = fake_actual(&shim);
        let notifier = RecordingNotifier::new();

        let mut state = State::default();
        let mut bank = AuthExpiredBank;
        let claude = FakeClaude { out: vec![] };
        run(
            &cfg, true, &mut state, &mut bank, &actual, &claude, &notifier, None, Confidence::High,
        )
        .await
        .unwrap();

        assert!(notifier.calls.lock().unwrap().is_empty(), "dry-run: no notify");
        assert!(!state_path.exists(), "dry-run auth-expired writes no state");
    }

    #[tokio::test]
    async fn dry_run_persists_only_rotated_token() {
        use chrono::TimeZone;
        let shim = tempfile::tempdir().unwrap();
        let statedir = tempfile::tempdir().unwrap();
        let state_path = statedir.path().join("s.toml");
        let cfg = cfg_at(&state_path);
        let actual = fake_actual(&shim);
        let notifier = NullNotifier;

        // Baseline on disk: a retained cache entry, a RESOLVED pending review, last_run, old token.
        // The review is resolved (resolve_to an existing payee) so the run mutates the IN-MEMORY
        // state (take_resolved removes the review + adds a cache entry). A broken splice that
        // persisted the whole in-memory state instead of just the token would then be caught.
        let baseline = State {
            last_run: Some(Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap()),
            monzo: crate::state::MonzoState {
                refresh_token: "old".into(),
                account_id: "acc".into(),
            },
            payee_cache: BTreeMap::from([("KEPT".to_string(), "Kept Payee".to_string())]),
            pending_reviews: vec![PendingReview {
                external_id: "p1".into(),
                raw_merchant: "RESOLVE_ME".into(),
                amount: -100,
                date: "2026-06-01".into(),
                resolve_to: "Local Cafe".into(), // existing payee in fake_actual
                create_new: false,
                suggestion: None,
                confidence: None,
                telegram_message_id: None,
            }],
            ..State::default()
        };
        baseline.save(&state_path).unwrap();

        let mut state = State::load(&state_path).unwrap();
        let mut bank = RotatingBank { rotate_to: "new".into() };
        let claude = FakeClaude { out: vec![] };
        run(
            &cfg, true, &mut state, &mut bank, &actual, &claude, &notifier, None, Confidence::High,
        )
        .await
        .unwrap();

        // Sanity: the in-memory state DID diverge (review resolved away) -> assertions are non-vacuous.
        assert!(
            state.pending_reviews.is_empty(),
            "in-memory review was resolved; test is non-vacuous"
        );

        let on_disk = State::load(&state_path).unwrap();
        assert_eq!(on_disk.payee_cache, baseline.payee_cache, "cache unchanged on disk");
        assert_eq!(on_disk.pending_reviews, baseline.pending_reviews, "pending unchanged on disk");
        assert_eq!(on_disk.last_run, baseline.last_run, "last_run not advanced");
        assert_eq!(on_disk.monzo.refresh_token, "new", "only the rotated token persisted");
    }

    #[tokio::test]
    async fn create_payee_import_failure_leaves_review_for_retry() {
        let shim = tempfile::tempdir().unwrap();
        let statedir = tempfile::tempdir().unwrap();
        let state_path = statedir.path().join("s.toml");
        let payees_log = shim.path().join("payees.log");
        let cfg = cfg_at(&state_path);
        let actual = fake_actual_failing_import(&shim);
        let notifier = NullNotifier;

        // A resolved create_new review (payee name falls back to raw_merchant).
        // Struct literal to avoid clippy::field_reassign_with_default.
        let mut state = State {
            monzo: crate::state::MonzoState {
                refresh_token: String::new(),
                account_id: "acc".into(),
            },
            pending_reviews: vec![PendingReview {
                external_id: "tx_x".into(),
                raw_merchant: "MyStore".into(),
                amount: -500,
                date: "2026-05-30".into(),
                resolve_to: String::new(),
                create_new: true,
                suggestion: None,
                confidence: None,
                telegram_message_id: None,
            }],
            ..State::default()
        };
        state.save(&state_path).unwrap(); // baseline on disk

        let mut bank = FakeBank { txs: vec![] };
        let claude = FakeClaude { out: vec![] };
        let result = run(
            &cfg, false, &mut state, &mut bank, &actual, &claude, &notifier, None, Confidence::High,
        )
        .await;

        let err = result.unwrap_err();
        assert!(
            format!("{err:#}").contains("import_transactions"),
            "error must originate from the import step, got: {err:#}"
        );
        assert!(payees_log.exists(), "create_payee was attempted (transient orphan)");
        let on_disk = State::load(&state_path).unwrap();
        assert_eq!(
            on_disk.pending_reviews.len(),
            1,
            "resolved review still on disk -> retry is clean (state not saved on failure)"
        );
        assert_eq!(on_disk.pending_reviews[0].external_id, "tx_x");
    }

    #[test]
    fn verify_state_accepts_exact_match() {
        assert!(super::verify_state("abc123", Some("abc123")).is_ok());
    }

    #[test]
    fn verify_state_rejects_mismatch() {
        assert!(super::verify_state("abc123", Some("nope")).is_err());
    }

    #[test]
    fn verify_state_rejects_missing() {
        assert!(super::verify_state("abc123", None).is_err());
    }

    #[test]
    fn random_state_is_32_hex_chars_and_varies() {
        let a = super::random_state().unwrap();
        let b = super::random_state().unwrap();
        assert_eq!(a.len(), 32, "128 bits => 32 hex chars");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "two nonces must differ");
    }
}
