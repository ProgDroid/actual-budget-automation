//! Run pipeline and auth bootstrap.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

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
use crate::state::{AccountLink, PendingReview, State};
use crate::telegram::{ConfirmChannel, Prompt};

/// Minimum normalised-name similarity for `aba accounts` to auto-link a Monzo
/// account to an Actual account; below this the row is left unmapped.
const ACCOUNT_MATCH_THRESHOLD: f64 = 0.6;

/// Resolve the Actual account id for a source Monzo account.
/// Falls back to the sole mapped account when there's exactly one (covers
/// single-account users and legacy held items with an empty source). Returns
/// `None` when the source is unknown and the mapping is ambiguous → caller holds.
fn actual_id_for(source_monzo_id: &str, links: &[AccountLink]) -> Option<String> {
    if let Some(l) = links
        .iter()
        .find(|l| l.monzo_id == source_monzo_id && !l.actual_id.is_empty())
    {
        return Some(l.actual_id.clone());
    }
    let mapped: Vec<&AccountLink> = links.iter().filter(|l| !l.actual_id.is_empty()).collect();
    if let [only] = mapped.as_slice() {
        Some(only.actual_id.clone())
    } else {
        None
    }
}

/// Result of splitting fetched transactions against the payee cache.
#[derive(Debug, PartialEq, Eq)]
pub struct ResolutionPlan {
    /// (`external_id`, `payee_name`) for cache hits.
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
#[derive(Debug, PartialEq, Eq)]
pub struct MatchOutcome {
    /// (`external_id`, `payee_name`) confident enough to import + cache now.
    pub matched: Vec<(String, String)>,
    /// (tx, `suggested_payee`, confidence) held for Telegram confirmation.
    pub proposed: Vec<(RawTransaction, String, Confidence)>,
    /// No usable match — held with no suggestion.
    pub unmatched: Vec<RawTransaction>,
}

// `matches` (input) and `matched` (auto-imported output) are distinct, well-named
// concepts here despite their lexical similarity.
#[allow(clippy::similar_names)]
pub fn apply_matches(
    txs: &[RawTransaction],
    matches: &[PayeeMatch],
    autoimport: Confidence,
    existing: &BTreeSet<&str>,
) -> MatchOutcome {
    let lookup: BTreeMap<&str, &PayeeMatch> = matches.iter().map(|m| (m.raw.as_str(), m)).collect();
    let mut matched = Vec::new();
    let mut proposed = Vec::new();
    let mut unmatched = Vec::new();
    for tx in txs {
        match lookup.get(tx.raw_merchant.as_str()) {
            Some(m) => match &m.payee {
                // Auto-import only a confident match to a payee that ACTUALLY
                // exists in Actual — otherwise it would be silently dropped at
                // import (no payee id), so hold it for review instead.
                Some(payee)
                    if m.confidence.rank() >= autoimport.rank()
                        && existing.contains(payee.as_str()) =>
                {
                    matched.push((tx.external_id.clone(), payee.clone()));
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
            let Some(date_str) = r.date.as_deref() else {
                warn!("schedule dropped: missing date (payee_id={:?})", r.payee_id);
                return None;
            };
            let date = match NaiveDate::parse_from_str(date_str, "%Y-%m-%d") {
                Ok(d) => d,
                Err(e) => {
                    warn!("schedule dropped: unparseable date '{date_str}': {e}");
                    return None;
                }
            };
            let Some(payee) = r.payee_id.as_deref().and_then(|id| by_id.get(id).copied()) else {
                warn!(
                    "schedule dropped: payee_id {:?} not in payee list",
                    r.payee_id
                );
                return None;
            };
            Some(Schedule {
                amount: r.amount,
                date,
                payee: payee.to_string(),
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
///
/// If the entry's `external_id` is too long to fit Telegram's 64-byte
/// `callback_data` cap, the keyboard is dropped entirely (every button shares
/// the same key, so it is all-or-nothing); the user resolves via a free-text
/// reply, which is correlated by `telegram_message_id` instead.
fn build_prompt(entry: &PendingReview, payee_names: &[String]) -> Prompt {
    use crate::telegram::encode_callback;
    let key = &entry.external_id;
    // Encode every (label, action) spec; if any key is too long they all are,
    // so collect into Option<Vec<_>> and fall back to no buttons.
    let buttons = |specs: &[(&str, &str)]| -> Vec<(String, String)> {
        specs
            .iter()
            .map(|(label, action)| {
                encode_callback(action, key).map(|data| (label.to_string(), data))
            })
            .collect::<Option<Vec<_>>>()
            .unwrap_or_default()
    };
    match &entry.suggestion {
        Some(s) if payee_names.iter().any(|p| p == s) => {
            let conf = entry.confidence.as_deref().unwrap_or("medium");
            Prompt {
                chat_text: format!(
                    "'{}' -> '{}'?  ({} confidence)\nReply with a different payee to remap.",
                    entry.raw_merchant, s, conf
                ),
                buttons: buttons(&[
                    ("✅ Confirm", "cf"),
                    ("🆕 Create new", "nw"),
                    ("❌ Skip", "sk"),
                ]),
            }
        }
        Some(s) => Prompt {
            chat_text: format!("No existing payee '{s}'. Create it?\nOr reply with a payee name."),
            buttons: buttons(&[("🆕 Create", "nw"), ("❌ Skip", "sk")]),
        },
        None => Prompt {
            chat_text: format!(
                "New merchant '{}' — which payee?\nReply with a payee name, or:",
                entry.raw_merchant
            ),
            buttons: buttons(&[("🆕 Create new", "nw"), ("❌ Skip", "sk")]),
        },
    }
}

/// Persist *only* the rotated Monzo refresh token, leaving the rest of the
/// on-disk state (payee cache, pending reviews, `last_run`) untouched. Monzo
/// invalidates the previous refresh
/// token on every successful refresh, so the new one must reach disk before any
/// fallible work; otherwise a later failure strands it and the next run replays
/// a dead token (Monzo answers 401). Re-loads the on-disk state and copies just
/// the token onto it.
fn persist_rotated_token(cfg: &Config, state: &State) -> anyhow::Result<()> {
    let mut on_disk = State::load(&cfg.state_path)?;
    on_disk
        .monzo
        .refresh_token
        .clone_from(&state.monzo.refresh_token);
    on_disk.save(&cfg.state_path)
}

/// Auto-suggest Monzo→Actual links by normalised-name similarity. Each Monzo
/// account is linked to its best Actual match above `threshold`; below it the
/// link is left unmapped (`actual_id` empty) for the user to fill in.
fn suggest_links(
    monzo: &[(String, String)],
    actual: &[(String, String)],
    threshold: f64,
) -> Vec<AccountLink> {
    monzo
        .iter()
        .map(|(mid, mname)| {
            let best = actual
                .iter()
                .map(|(aid, aname)| (aid, aname, crate::matching::payee_similarity(mname, aname)))
                .filter(|(_, _, score)| *score >= threshold)
                .max_by(|a, b| a.2.total_cmp(&b.2));
            match best {
                Some((aid, aname, _)) => AccountLink {
                    monzo_id: mid.clone(),
                    monzo_name: mname.clone(),
                    actual_id: aid.clone(),
                    actual_name: aname.clone(),
                },
                None => AccountLink {
                    monzo_id: mid.clone(),
                    monzo_name: mname.clone(),
                    ..AccountLink::default()
                },
            }
        })
        .collect()
}

/// `aba accounts [--write]`: fetch Monzo accounts/pots + Actual accounts,
/// auto-suggest a mapping, print it, and (with `--write`) persist to state.
/// Run where it can reach Monzo + the Actual shim; review the printed mapping
/// and override by editing `[[monzo.accounts]]` in the state file.
#[allow(clippy::future_not_send)]
pub async fn map_accounts(cfg: &Config, write: bool) -> anyhow::Result<()> {
    let _lock = crate::lock::lock_blocking(&cfg.lock_path)?;
    let mut state = State::load(&cfg.state_path)?;

    let mut bank = crate::banks::monzo::Monzo::new(
        cfg.monzo_client_id.clone(),
        cfg.monzo_client_secret.clone(),
        cfg.monzo_base_url.clone(),
        state.monzo_clone(),
    );
    match bank.ensure_auth(&mut state).await {
        Ok(()) => persist_rotated_token(cfg, &state)?,
        Err(BankError::AuthExpired(m)) => {
            return Err(anyhow::anyhow!(
                "Monzo auth expired: {m}. Run `aba auth monzo` first."
            ));
        }
        Err(BankError::Other(e)) => return Err(e),
    }

    let monzo_accounts = bank.list_accounts().await.map_err(map_bank_err)?;
    let actual = ActualClient::new(
        cfg.shim_dir.clone(),
        true, // read-only: we only call get_accounts
        cfg.actual_server_url.clone(),
        cfg.actual_password.clone(),
        cfg.actual_sync_id.clone(),
    );
    let actual_accounts: Vec<(String, String)> = actual
        .get_accounts()
        .await
        .context("get_accounts")?
        .into_iter()
        .map(|a| (a.id, a.name))
        .collect();

    let links = suggest_links(&monzo_accounts, &actual_accounts, ACCOUNT_MATCH_THRESHOLD);

    println!("Suggested Monzo → Actual account mapping:");
    for l in &links {
        if l.actual_id.is_empty() {
            println!(
                "  {} ({})  →  [UNMAPPED — edit state file]",
                l.monzo_name, l.monzo_id
            );
        } else {
            println!(
                "  {} ({})  →  {} ({})",
                l.monzo_name, l.monzo_id, l.actual_name, l.actual_id
            );
        }
    }

    // Informational: pots are Phase 3 (transfers), not linked here.
    for (mid, _) in &monzo_accounts {
        match bank.list_pots(mid).await {
            Ok(pots) if !pots.is_empty() => {
                println!("Pots under {mid} (Phase 3 — not linked yet):");
                for (pid, pname) in pots {
                    println!("  {pname} ({pid})");
                }
            }
            Ok(_) => {}
            Err(e) => warn!("could not list pots for {mid}: {}", bank_err_text(&e)),
        }
    }

    if write {
        state.monzo.accounts = links;
        state.save(&cfg.state_path)?;
        println!(
            "\nWrote {} link(s) to {}.",
            state.monzo.accounts.len(),
            cfg.state_path.display()
        );
        println!("Review/override any UNMAPPED or wrong rows by editing [[monzo.accounts]] there.");
    } else {
        println!("\n(dry run — re-run with `aba accounts --write` to persist)");
    }
    Ok(())
}

fn map_bank_err(e: BankError) -> anyhow::Error {
    match e {
        BankError::Other(e) => e,
        BankError::AuthExpired(m) => anyhow::anyhow!("Monzo auth expired: {m}"),
    }
}

fn bank_err_text(e: &BankError) -> String {
    match e {
        BankError::Other(e) => e.to_string(),
        BankError::AuthExpired(m) => format!("auth expired: {m}"),
    }
}

// Top-level orchestration: deliberately long and wired to many collaborators.
// It is never spawned across threads (driven directly under a timeout in main),
// so `future_not_send` does not apply.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::future_not_send
)]
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
    // 1. Auth. Monzo rotates the refresh token on every successful refresh and
    // invalidates the previous one server-side, so persist the rotated token to
    // disk IMMEDIATELY — before any fallible fetch/Actual work. Otherwise a later
    // failure strands the new token and the next run replays a dead one (401).
    match bank.ensure_auth(state).await {
        Ok(()) => persist_rotated_token(cfg, state)?,
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
    // Cursor for a successful run: stamp BEFORE fetching so a transaction that
    // arrives mid-run is re-fetched next time (dedup covers the overlap) rather
    // than falling into a gap and being lost (M1).
    let fetch_start = Utc::now();
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
            if let Some(canon) = best_payee_match(&pr.resolve_to, &payee_names, PAYEE_THRESHOLD) {
                pr.resolve_to = canon;
            } else {
                // Free-text remap didn't match a real payee: re-hold as a
                // "create it?" suggestion and re-prompt next. Edit the stale
                // inline-keyboard message first so its old buttons can't be
                // tapped (best-effort; failure must not abort the run).
                if let (Some(ch), Some(old_id)) = (channel, pr.telegram_message_id) {
                    let text = format!("↪️ Couldn't match '{}' — re-asking below.", pr.resolve_to);
                    if let Err(e) = ch.edit_message_text(old_id, &text).await {
                        warn!("telegram edit_message_text failed (stale prompt left in chat): {e}");
                    }
                }
                pr.suggestion = Some(std::mem::take(&mut pr.resolve_to));
                pr.telegram_message_id = None;
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
    let existing: BTreeSet<&str> = payee_names.iter().map(String::as_str).collect();
    let outcome = apply_matches(&unknown_txs, &claude_matches, autoimport, &existing);

    // Clone links so the routing closure/holds don't fight the borrow checker
    // against `state` mutations below. Cheap: a handful of entries.
    let account_links = state.monzo.accounts.clone();

    // 6. Assemble import rows (payee NAME -> id; never auto-create on the matched path).
    let mut grouped: BTreeMap<String, Vec<ImportTxn>> = BTreeMap::new();
    let mut held = 0usize;

    let make_row = |tx: &RawTransaction, payee_id: &str| ImportTxn {
        date: tx.date.format("%Y-%m-%d").to_string(),
        amount: tx.amount_minor,
        payee: payee_id.to_string(),
        category: None,
        imported_id: tx.external_id.clone(),
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
            (Some(tx), Some(pid)) => {
                if let Some(aid) = actual_id_for(&tx.source_monzo_id, &account_links) {
                    grouped.entry(aid).or_default().push(make_row(tx, pid));
                } else {
                    warn!(
                        "tx {ext_id} from unmapped Monzo account '{}' — holding (run `aba accounts`)",
                        tx.source_monzo_id
                    );
                    let date = tx.date.format("%Y-%m-%d").to_string();
                    if state.add_pending(
                        &tx.external_id,
                        &tx.raw_merchant,
                        tx.amount_minor,
                        &date,
                        &tx.source_monzo_id,
                    ) {
                        held += 1;
                    }
                }
            }
            (Some(tx), None) => {
                warn!(
                    "cached payee '{payee_name}' for tx {ext_id} no longer exists in Actual; holding for review"
                );
                let date = tx.date.format("%Y-%m-%d").to_string();
                if state.add_pending_full(
                    &tx.external_id,
                    &tx.raw_merchant,
                    tx.amount_minor,
                    &date,
                    Some(payee_name.clone()),
                    None,
                    &tx.source_monzo_id,
                ) {
                    held += 1;
                }
            }
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
                if let Some(aid) = actual_id_for(&tx.source_monzo_id, &account_links) {
                    grouped.entry(aid).or_default().push(make_row(tx, pid));
                    if !state.payee_cache.contains_key(&tx.raw_merchant) {
                        state
                            .payee_cache
                            .insert(tx.raw_merchant.clone(), payee_name.clone());
                        newly_cached.push((tx.raw_merchant.clone(), payee_name.clone()));
                    }
                } else {
                    warn!(
                        "matched tx {ext_id} from unmapped Monzo account '{}' — holding (run `aba accounts`)",
                        tx.source_monzo_id
                    );
                    let date = tx.date.format("%Y-%m-%d").to_string();
                    if state.add_pending(
                        &tx.external_id,
                        &tx.raw_merchant,
                        tx.amount_minor,
                        &date,
                        &tx.source_monzo_id,
                    ) {
                        held += 1;
                    }
                }
            }
            (Some(tx), None) => {
                // Defensive: apply_matches guarantees matched payees exist. If
                // this ever fires, hold for review rather than dropping (fail-loud).
                warn!(
                    "matched payee '{payee_name}' for tx {ext_id} unexpectedly missing; holding for review"
                );
                let date = tx.date.format("%Y-%m-%d").to_string();
                if state.add_pending_full(
                    &tx.external_id,
                    &tx.raw_merchant,
                    tx.amount_minor,
                    &date,
                    Some(payee_name.clone()),
                    None,
                    &tx.source_monzo_id,
                ) {
                    held += 1;
                }
            }
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
        if let Some(aid) = actual_id_for(&r.source_monzo_id, &account_links) {
            grouped.entry(aid).or_default().push(ImportTxn {
                date: r.date.clone(),
                amount: r.amount,
                payee: pid,
                category: None,
                imported_id: if r.external_id.is_empty() {
                    format!("{}:{}", r.raw_merchant, r.date)
                } else {
                    r.external_id.clone()
                },
            });
        } else {
            // Fail-loud: the user resolved this, but its source Monzo account is
            // still unmapped, so we cannot route the import. Re-hold it (rather
            // than dropping — take_resolved already removed it from pending) so
            // it survives until `aba accounts` maps the account. Preserve the
            // resolved payee as the suggestion so it can be confirmed next time.
            warn!(
                "resolved review '{}' from unmapped Monzo account '{}' — re-holding until mapped (run `aba accounts`)",
                r.raw_merchant, r.source_monzo_id
            );
            if state.add_pending_full(
                &r.external_id,
                &r.raw_merchant,
                r.amount,
                &r.date,
                Some(r.payee_name.clone()),
                None,
                &r.source_monzo_id,
            ) {
                held += 1;
            }
        }
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
            &tx.source_monzo_id,
        ) {
            held += 1;
        }
    }
    // unmatched -> held with no suggestion
    for tx in &outcome.unmatched {
        let date = tx.date.format("%Y-%m-%d").to_string();
        if state.add_pending(
            &tx.external_id,
            &tx.raw_merchant,
            tx.amount_minor,
            &date,
            &tx.source_monzo_id,
        ) {
            held += 1;
        }
    }

    // 7. Import or report.
    if dry_run {
        let total: usize = grouped.values().map(Vec::len).sum();
        println!("DRY RUN — no writes to Actual");
        println!("  would import : {total}");
        println!(
            "  would skip   : {} (matched a schedule)",
            fetched_count.saturating_sub(candidates.len())
        );
        println!("  would hold   : {held}");
        for (aid, rows) in &grouped {
            println!("  -> account {aid}: {} txns", rows.len());
            for r in rows {
                println!("    + {} {} -> payee {}", r.date, r.amount, r.payee);
            }
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
        // Re-persist only the rotated token (see persist_rotated_token); discard
        // the in-memory cache/pending/last_run changes a dry-run must not commit.
        persist_rotated_token(cfg, state)?;
        return Ok(());
    }

    let mut summary = crate::actual::ImportSummary {
        added: 0,
        updated: 0,
    };
    for (aid, rows) in grouped {
        if rows.is_empty() {
            continue;
        }
        let s = actual
            .import_transactions(aid.clone(), rows)
            .await
            .with_context(|| format!("import_transactions for account {aid}"))?;
        summary.added += s.added;
        summary.updated += s.updated;
    }
    info!(
        "imported added={} updated={} held={held}",
        summary.added, summary.updated
    );

    state.last_run = Some(fetch_start);

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
                Ok(message_id) => {
                    state.pending_reviews[i].telegram_message_id = Some(message_id);
                    // Persist now so a later failure can't strand this id (M6).
                    if let Err(e) = state.save(&cfg.state_path) {
                        warn!(
                            "failed to persist telegram_message_id (may re-prompt next run): {e}"
                        );
                    }
                }
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
        let _ = write!(body, "\nNewly auto-matched ({}):", newly_cached.len());
        for (raw, payee) in &newly_cached {
            let _ = write!(body, "\n  {raw} -> {payee}");
        }
    }
    if awaiting > 0 {
        let _ = write!(body, "\n{awaiting} item(s) awaiting your confirmation.");
    }
    notifier.notify("Actual import complete", &body).await.ok();

    Ok(())
}

/// Generate an unguessable 128-bit OAuth `state` nonce, hex-encoded (32 chars).
fn random_state() -> anyhow::Result<String> {
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf).map_err(|e| anyhow::anyhow!("RNG failure generating state: {e}"))?;
    Ok(buf.iter().fold(String::with_capacity(32), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    }))
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

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::actual::ActualClient;
    use crate::banks::{BankError, BankSource, RawTransaction};
    use crate::claude::{ClaudeMatcher, Confidence, PayeeMatch};
    use crate::notify::Notifier;
    use crate::state::PendingReview;
    use crate::telegram::{ConfirmChannel, Prompt, Reply};
    use async_trait::async_trait;
    use chrono::{DateTime, NaiveDate, Utc};
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Mutex;
    use tempfile::TempDir;

    pub fn tx(id: &str, merchant: &str, amount: i64) -> RawTransaction {
        RawTransaction {
            external_id: id.into(),
            date: NaiveDate::from_ymd_opt(2026, 5, 30).unwrap(),
            amount_minor: amount,
            raw_merchant: merchant.into(),
            source_monzo_id: "acc_1".into(),
        }
    }

    /// Test double: returns scripted replies once, records sent prompts and edits.
    pub struct FakeChannel {
        pub scripted: Mutex<Vec<Reply>>,
        pub offset: i64,
        pub sent: Mutex<Vec<Prompt>>,
        pub next_id: Mutex<i64>,
        pub edited: Mutex<Vec<(i64, String)>>,
        pub edit_should_fail: bool,
    }
    impl FakeChannel {
        pub fn new(scripted: Vec<Reply>, offset: i64) -> Self {
            Self {
                scripted: Mutex::new(scripted),
                offset,
                sent: Mutex::new(Vec::new()),
                next_id: Mutex::new(1000),
                edited: Mutex::new(Vec::new()),
                edit_should_fail: false,
            }
        }

        /// Variant whose `edit_message_text` always errors, to prove the run
        /// continues regardless.
        pub fn with_failing_edit(mut self) -> Self {
            self.edit_should_fail = true;
            self
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
        async fn edit_message_text(&self, message_id: i64, text: &str) -> anyhow::Result<()> {
            self.edited
                .lock()
                .unwrap()
                .push((message_id, text.to_string()));
            if self.edit_should_fail {
                return Err(anyhow::anyhow!("edit failed (test)"));
            }
            Ok(())
        }
    }

    #[test]
    fn actual_id_for_matches_source_then_falls_back_to_sole_mapped() {
        let links = vec![
            AccountLink {
                monzo_id: "acc_1".into(),
                actual_id: "u1".into(),
                ..Default::default()
            },
            AccountLink {
                monzo_id: "acc_2".into(),
                actual_id: "u2".into(),
                ..Default::default()
            },
        ];
        assert_eq!(actual_id_for("acc_2", &links).as_deref(), Some("u2"));
        assert_eq!(actual_id_for("acc_unknown", &links), None); // ambiguous → hold

        let single = vec![AccountLink {
            monzo_id: "acc_1".into(),
            actual_id: "u1".into(),
            ..Default::default()
        }];
        assert_eq!(actual_id_for("", &single).as_deref(), Some("u1")); // legacy fallback
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
        let existing: BTreeSet<&str> = ["Tesco", "Local Cafe"].iter().copied().collect();
        let outcome = apply_matches(&txs, &matches, Confidence::High, &existing);
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
            source_monzo_id: String::new(),
            raw_merchant: "SQ *CAFE".into(),
            amount: -450,
            date: "2026-06-01".into(),
            resolve_to: String::new(),
            create_new: false,
            suggestion: Some("Local Cafe".into()),
            confidence: Some("medium".into()),
            category_suggestion: None,
            category_confidence: None,
            resolve_category_to: String::new(),
            category_resolved: true,
            telegram_message_id: None,
        };
        let p = build_prompt(&proposed, &payees);
        assert!(
            p.chat_text.contains("'Local Cafe'"),
            "expected plain-text quoting: {}",
            p.chat_text
        );
        assert!(
            !p.chat_text.contains("*Local Cafe*"),
            "must not use Markdown bold for payee: {}",
            p.chat_text
        );
        assert_eq!(p.buttons.len(), 3);
        assert_eq!(p.buttons[0].1, "cf:tx1");

        // no-match -> create/skip only
        let unmatched = PendingReview {
            external_id: "tx2".into(),
            source_monzo_id: String::new(),
            raw_merchant: "MYSTERY".into(),
            amount: -10,
            date: "2026-06-01".into(),
            resolve_to: String::new(),
            create_new: false,
            suggestion: None,
            confidence: None,
            category_suggestion: None,
            category_confidence: None,
            resolve_category_to: String::new(),
            category_resolved: true,
            telegram_message_id: None,
        };
        let p2 = build_prompt(&unmatched, &payees);
        assert_eq!(p2.buttons.len(), 2);
        assert_eq!(p2.buttons[0].1, "nw:tx2");

        // failed remap: suggestion set but NOT in payees -> "create it?" flavor
        let failed = PendingReview {
            external_id: "tx3".into(),
            source_monzo_id: String::new(),
            raw_merchant: "X".into(),
            amount: -10,
            date: "2026-06-01".into(),
            resolve_to: String::new(),
            create_new: false,
            suggestion: Some("Sainsbury's".into()),
            confidence: None,
            category_suggestion: None,
            category_confidence: None,
            resolve_category_to: String::new(),
            category_resolved: true,
            telegram_message_id: None,
        };
        let p3 = build_prompt(&failed, &payees);
        assert!(p3.chat_text.contains("Sainsbury's"));
        assert_eq!(p3.buttons.len(), 2);
        assert_eq!(p3.buttons[0].1, "nw:tx3"); // create
        assert_eq!(p3.buttons[1].1, "sk:tx3"); // skip
    }

    #[test]
    fn build_prompt_drops_buttons_when_external_id_too_long() {
        // 80-char external_id + "cf:" overflows Telegram's 64-byte callback cap,
        // so the prompt must carry no buttons (free-text reply still works).
        let long_id = "z".repeat(80);
        let payees = vec!["Local Cafe".to_string()];
        let entry = PendingReview {
            external_id: long_id,
            source_monzo_id: String::new(),
            raw_merchant: "SQ *CAFE".into(),
            amount: -450,
            date: "2026-06-01".into(),
            resolve_to: String::new(),
            create_new: false,
            suggestion: Some("Local Cafe".into()),
            confidence: Some("medium".into()),
            category_suggestion: None,
            category_confidence: None,
            resolve_category_to: String::new(),
            category_resolved: true,
            telegram_message_id: None,
        };
        let p = build_prompt(&entry, &payees);
        assert!(
            p.buttons.is_empty(),
            "over-long external_id must yield no buttons, got: {:?}",
            p.buttons
        );
        assert!(!p.chat_text.is_empty(), "chat text must still be present");
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
            Self {
                calls: Mutex::new(Vec::new()),
            }
        }
    }
    #[async_trait]
    impl Notifier for RecordingNotifier {
        async fn notify(&self, t: &str, b: &str) -> anyhow::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push((t.to_string(), b.to_string()));
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
        let script = r"
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
";
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
        let script = r"
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
";
        std::fs::write(dir.path().join("index.js"), script).unwrap();
        ActualClient::new(
            dir.path().to_path_buf(),
            false,
            "http://t".into(),
            "pw".into(),
            "sync".into(),
        )
    }

    /// Like `fake_actual` but `get_payees` fails — simulates the Actual server
    /// being unreachable *after* a successful Monzo refresh (the exact path that
    /// silently burned rotated refresh tokens).
    fn fake_actual_failing_get_payees(dir: &TempDir) -> ActualClient {
        let script = r"
let b='';process.stdin.on('data',c=>b+=c);process.stdin.on('end',()=>{
  const req=JSON.parse(b); const out=o=>process.stdout.write(JSON.stringify(o));
  if(req.op==='get_payees'){ out({ok:false,error:'network-failure'}); }
  else { out({ok:false,error:'unknown op'}); }
});
";
        std::fs::write(dir.path().join("index.js"), script).unwrap();
        ActualClient::new(
            dir.path().to_path_buf(),
            false,
            "http://t".into(),
            "pw".into(),
            "sync".into(),
        )
    }

    /// Like `fake_actual` but exposes TWO payees (Local Cafe + Bills Co) and
    /// records each `import_transactions` call as one JSON line
    /// `{"account_id":..,"transactions":[..]}` in `<dir>/routed.log`, so a test
    /// can assert which Actual account each transaction was routed to.
    fn fake_actual_routing(dir: &TempDir) -> ActualClient {
        let script = r"
let b='';process.stdin.on('data',c=>b+=c);process.stdin.on('end',()=>{
  const req=JSON.parse(b); const out=o=>process.stdout.write(JSON.stringify(o));
  const fs=require('fs'), path=require('path');
  if(req.op==='get_payees'){ out({ok:true,data:[{id:'p_cafe',name:'Local Cafe'},{id:'p_bills',name:'Bills Co'}]}); }
  else if(req.op==='get_scheduled'){ out({ok:true,data:[]}); }
  else if(req.op==='create_payee'){ out({ok:true,data:{id:'p_new'}}); }
  else if(req.op==='import_transactions'){
    fs.appendFileSync(path.join(__dirname,'routed.log'), JSON.stringify({account_id:req.account_id,transactions:req.transactions})+'\n');
    out({ok:true,data:{added:req.transactions.length,updated:0}});
  } else { out({ok:false,error:'unknown op'}); }
});
";
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
        // tx() tags source_monzo_id "acc_1"; map it so the eventual import routes.
        state.monzo.accounts.push(crate::state::AccountLink {
            monzo_id: "acc_1".into(),
            actual_id: "acc".into(),
            ..Default::default()
        });
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
    async fn imports_route_to_per_source_actual_account() {
        // Two mapped accounts (acc_1->u1, acc_2->u2). Two high-confidence txns,
        // one from each Monzo account, must be imported into the matching Actual
        // account via separate import_transactions calls.
        let shim = tempfile::tempdir().unwrap();
        let statedir = tempfile::tempdir().unwrap();
        let state_path = statedir.path().join("s.toml");
        let routed_log = shim.path().join("routed.log");
        let cfg = cfg_at(&state_path);
        let actual = fake_actual_routing(&shim);
        let notifier = NullNotifier;

        let mut state = State::default();
        state.monzo.accounts.push(AccountLink {
            monzo_id: "acc_1".into(),
            actual_id: "u1".into(),
            ..Default::default()
        });
        state.monzo.accounts.push(AccountLink {
            monzo_id: "acc_2".into(),
            actual_id: "u2".into(),
            ..Default::default()
        });

        // tx() tags acc_1; build the acc_2 one explicitly.
        let tx_cafe = tx("tx_cafe", "SQ *CAFE", -450); // source acc_1
        let tx_bills = RawTransaction {
            external_id: "tx_bills".into(),
            date: NaiveDate::from_ymd_opt(2026, 5, 30).unwrap(),
            amount_minor: -1200,
            raw_merchant: "BILLS LTD".into(),
            source_monzo_id: "acc_2".into(),
        };
        let mut bank = FakeBank {
            txs: vec![tx_cafe, tx_bills],
        };
        let claude = FakeClaude {
            out: vec![
                PayeeMatch {
                    raw: "SQ *CAFE".into(),
                    payee: Some("Local Cafe".into()),
                    confidence: Confidence::High,
                },
                PayeeMatch {
                    raw: "BILLS LTD".into(),
                    payee: Some("Bills Co".into()),
                    confidence: Confidence::High,
                },
            ],
        };
        run(
            &cfg,
            false,
            &mut state,
            &mut bank,
            &actual,
            &claude,
            &notifier,
            None,
            Confidence::High,
        )
        .await
        .unwrap();

        // One import call per account, each carrying only its own transaction.
        let logged = std::fs::read_to_string(&routed_log).unwrap();
        let mut calls: Vec<serde_json::Value> = logged
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(calls.len(), 2, "exactly one import call per account");
        calls.sort_by(|a, b| {
            a["account_id"]
                .as_str()
                .unwrap()
                .cmp(b["account_id"].as_str().unwrap())
        });

        assert_eq!(calls[0]["account_id"], "u1");
        assert_eq!(calls[0]["transactions"].as_array().unwrap().len(), 1);
        assert_eq!(calls[0]["transactions"][0]["imported_id"], "tx_cafe");
        assert_eq!(calls[0]["transactions"][0]["payee"], "p_cafe");

        assert_eq!(calls[1]["account_id"], "u2");
        assert_eq!(calls[1]["transactions"].as_array().unwrap().len(), 1);
        assert_eq!(calls[1]["transactions"][0]["imported_id"], "tx_bills");
        assert_eq!(calls[1]["transactions"][0]["payee"], "p_bills");
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
            &cfg,
            false,
            &mut state,
            &mut bank,
            &actual,
            &claude,
            &notifier,
            None,
            Confidence::High,
        )
        .await
        .unwrap();

        let calls = notifier.calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "one re-auth notification");
        assert_eq!(calls[0].0, "Monzo re-auth required");
        drop(calls);
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
            &cfg,
            true,
            &mut state,
            &mut bank,
            &actual,
            &claude,
            &notifier,
            None,
            Confidence::High,
        )
        .await
        .unwrap();

        assert!(
            notifier.calls.lock().unwrap().is_empty(),
            "dry-run: no notify"
        );
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
                accounts: Vec::new(),
            },
            payee_cache: BTreeMap::from([("KEPT".to_string(), "Kept Payee".to_string())]),
            pending_reviews: vec![PendingReview {
                external_id: "p1".into(),
                source_monzo_id: String::new(),
                raw_merchant: "RESOLVE_ME".into(),
                amount: -100,
                date: "2026-06-01".into(),
                resolve_to: "Local Cafe".into(), // existing payee in fake_actual
                create_new: false,
                suggestion: None,
                confidence: None,
                category_suggestion: None,
                category_confidence: None,
                resolve_category_to: String::new(),
                category_resolved: true,
                telegram_message_id: None,
            }],
            ..State::default()
        };
        baseline.save(&state_path).unwrap();

        let mut state = State::load(&state_path).unwrap();
        let mut bank = RotatingBank {
            rotate_to: "new".into(),
        };
        let claude = FakeClaude { out: vec![] };
        run(
            &cfg,
            true,
            &mut state,
            &mut bank,
            &actual,
            &claude,
            &notifier,
            None,
            Confidence::High,
        )
        .await
        .unwrap();

        // Sanity: the in-memory state DID diverge (take_resolved folded the
        // resolved raw->payee into the cache) -> assertions are non-vacuous.
        // The review's source account is unmapped, so it is RE-HELD in memory
        // (fail-loud) rather than imported; either way the on-disk state below
        // must reflect ONLY the rotated token, never these in-memory changes.
        assert!(
            state.payee_cache.contains_key("RESOLVE_ME"),
            "in-memory cache gained the resolved mapping; test is non-vacuous"
        );

        let on_disk = State::load(&state_path).unwrap();
        assert_eq!(
            on_disk.payee_cache, baseline.payee_cache,
            "cache unchanged on disk"
        );
        assert_eq!(
            on_disk.pending_reviews, baseline.pending_reviews,
            "pending unchanged on disk"
        );
        assert_eq!(on_disk.last_run, baseline.last_run, "last_run not advanced");
        assert_eq!(
            on_disk.monzo.refresh_token, "new",
            "only the rotated token persisted"
        );
    }

    #[tokio::test]
    async fn rotated_token_persisted_when_later_step_fails() {
        // Regression for the Monzo lock-out bug: `ensure_auth` rotates the refresh
        // token (Monzo invalidates the previous one server-side), but a later step
        // — here `get_payees` — fails and `run` returns Err. The rotated token MUST
        // already be on disk; otherwise the next run replays the dead token and
        // Monzo answers 401, locking the user out until a manual re-auth.
        let shim = tempfile::tempdir().unwrap();
        let statedir = tempfile::tempdir().unwrap();
        let state_path = statedir.path().join("s.toml");
        let cfg = cfg_at(&state_path);
        let actual = fake_actual_failing_get_payees(&shim);
        let notifier = NullNotifier;

        let baseline = State {
            monzo: crate::state::MonzoState {
                refresh_token: "old".into(),
                account_id: "acc".into(),
                accounts: Vec::new(),
            },
            ..State::default()
        };
        baseline.save(&state_path).unwrap();

        let mut state = State::load(&state_path).unwrap();
        let mut bank = RotatingBank {
            rotate_to: "new".into(),
        };
        let claude = FakeClaude { out: vec![] };
        let result = run(
            &cfg,
            false,
            &mut state,
            &mut bank,
            &actual,
            &claude,
            &notifier,
            None,
            Confidence::High,
        )
        .await;

        assert!(result.is_err(), "run should fail at get_payees");
        let on_disk = State::load(&state_path).unwrap();
        assert_eq!(
            on_disk.monzo.refresh_token, "new",
            "rotated refresh token must be persisted before the failing step"
        );
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
                accounts: vec![crate::state::AccountLink {
                    monzo_id: "acc_1".into(),
                    actual_id: "acc".into(),
                    ..Default::default()
                }],
            },
            pending_reviews: vec![PendingReview {
                external_id: "tx_x".into(),
                source_monzo_id: "acc_1".into(),
                raw_merchant: "MyStore".into(),
                amount: -500,
                date: "2026-05-30".into(),
                resolve_to: String::new(),
                create_new: true,
                suggestion: None,
                confidence: None,
                category_suggestion: None,
                category_confidence: None,
                resolve_category_to: String::new(),
                category_resolved: true,
                telegram_message_id: None,
            }],
            ..State::default()
        };
        state.save(&state_path).unwrap(); // baseline on disk

        let mut bank = FakeBank { txs: vec![] };
        let claude = FakeClaude { out: vec![] };
        let result = run(
            &cfg,
            false,
            &mut state,
            &mut bank,
            &actual,
            &claude,
            &notifier,
            None,
            Confidence::High,
        )
        .await;

        let err = result.unwrap_err();
        assert!(
            format!("{err:#}").contains("import_transactions"),
            "error must originate from the import step, got: {err:#}"
        );
        assert!(
            payees_log.exists(),
            "create_payee was attempted (transient orphan)"
        );
        let on_disk = State::load(&state_path).unwrap();
        assert_eq!(
            on_disk.pending_reviews.len(),
            1,
            "resolved review still on disk -> retry is clean (state not saved on failure)"
        );
        assert_eq!(on_disk.pending_reviews[0].external_id, "tx_x");
    }

    #[tokio::test]
    async fn failed_remap_edits_stale_prompt_then_reprompts() {
        let shim = tempfile::tempdir().unwrap();
        let statedir = tempfile::tempdir().unwrap();
        let state_path = statedir.path().join("s.toml");
        let cfg = cfg_at(&state_path);
        let actual = fake_actual(&shim); // get_payees -> [Local Cafe]
        let notifier = NullNotifier;
        let channel = FakeChannel::new(vec![], 0);

        // A held entry whose free-text remap ("Nonexistent Payee") will NOT match
        // the live payee list, with an existing prompt message_id (500).
        let mut state = State::default();
        state.add_pending_full(
            "tx1",
            "SQ *CAFE",
            -450,
            "2026-06-01",
            Some("Local Cafe".into()),
            Some("medium".into()),
            "",
        );
        state.pending_reviews[0].telegram_message_id = Some(500);
        assert!(state.remap_pending("tx1", "Nonexistent Payee"));

        let mut bank = FakeBank { txs: vec![] };
        let claude = FakeClaude { out: vec![] };
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

        // Stale prompt (id 500) was edited exactly once.
        let edited = channel.edited.lock().unwrap();
        assert_eq!(edited.len(), 1);
        assert_eq!(edited[0].0, 500);
        drop(edited);

        // Entry re-held as a "create it?" suggestion, resolve_to cleared, and a
        // fresh prompt was sent (new message_id, recorded in `sent`).
        assert_eq!(state.pending_reviews.len(), 1);
        assert_eq!(
            state.pending_reviews[0].suggestion.as_deref(),
            Some("Nonexistent Payee")
        );
        assert!(state.pending_reviews[0].resolve_to.is_empty());
        assert_eq!(channel.sent.lock().unwrap().len(), 1);
        assert_eq!(state.pending_reviews[0].telegram_message_id, Some(1001));
    }

    #[tokio::test]
    async fn failed_remap_edit_failure_does_not_abort_run() {
        let shim = tempfile::tempdir().unwrap();
        let statedir = tempfile::tempdir().unwrap();
        let state_path = statedir.path().join("s.toml");
        let cfg = cfg_at(&state_path);
        let actual = fake_actual(&shim);
        let notifier = NullNotifier;
        let channel = FakeChannel::new(vec![], 0).with_failing_edit();

        let mut state = State::default();
        state.add_pending_full(
            "tx1",
            "SQ *CAFE",
            -450,
            "2026-06-01",
            Some("Local Cafe".into()),
            Some("medium".into()),
            "",
        );
        state.pending_reviews[0].telegram_message_id = Some(500);
        assert!(state.remap_pending("tx1", "Nonexistent Payee"));

        let mut bank = FakeBank { txs: vec![] };
        let claude = FakeClaude { out: vec![] };
        // Must complete Ok despite the edit error.
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

        // Edit was attempted, and the entry was still re-held + re-prompted.
        assert_eq!(channel.edited.lock().unwrap().len(), 1);
        assert_eq!(
            state.pending_reviews[0].suggestion.as_deref(),
            Some("Nonexistent Payee")
        );
    }

    #[tokio::test]
    async fn high_confidence_match_to_nonexistent_payee_is_held_not_dropped() {
        let shim = tempfile::tempdir().unwrap();
        let statedir = tempfile::tempdir().unwrap();
        let state_path = statedir.path().join("s.toml");
        let imports_log = shim.path().join("imports.log");
        let cfg = cfg_at(&state_path);
        let actual = fake_actual(&shim); // get_payees -> [Local Cafe]
        let notifier = NullNotifier;
        let channel = FakeChannel::new(vec![], 0);

        let mut state = State::default();
        let mut bank = FakeBank {
            txs: vec![tx("tx_ghost", "SQ *GHOST", -999)],
        };
        // Claude returns a HIGH-confidence payee that does NOT exist in Actual.
        let claude = FakeClaude {
            out: vec![PayeeMatch {
                raw: "SQ *GHOST".into(),
                payee: Some("Ghost Payee".into()),
                confidence: Confidence::High,
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

        // Not imported...
        assert!(
            !imports_log.exists(),
            "non-existent payee must NOT be imported"
        );
        // ...held for review with the proposed name as the suggestion...
        assert_eq!(state.pending_reviews.len(), 1);
        assert_eq!(state.pending_reviews[0].external_id, "tx_ghost");
        assert_eq!(
            state.pending_reviews[0].suggestion.as_deref(),
            Some("Ghost Payee")
        );
        // ...and last_run still advanced (the tx is safely held, not lost).
        assert!(state.last_run.is_some());
    }

    #[tokio::test]
    async fn resolved_review_from_unmapped_account_is_reheld_not_dropped() {
        // Fail-loud regression: a user-resolved review whose source Monzo account
        // is still unmapped must be RE-HELD (not silently dropped). take_resolved
        // removes it from pending before the import loop; if actual_id_for returns
        // None and we merely warn+skip, the successful save persists the removal
        // and the transaction is lost forever.
        let shim = tempfile::tempdir().unwrap();
        let statedir = tempfile::tempdir().unwrap();
        let state_path = statedir.path().join("s.toml");
        let imports_log = shim.path().join("imports.log");
        let cfg = cfg_at(&state_path);
        let actual = fake_actual(&shim); // get_payees -> [Local Cafe]
        let notifier = NullNotifier;
        let channel = FakeChannel::new(vec![], 0);

        // TWO mapped links (acc_1->u1, acc_2->u2): >1 mapped so actual_id_for has
        // no sole-fallback, and the resolved item's source (acc_unmapped) matches
        // neither -> actual_id_for returns None -> must re-hold.
        let mut state = State::default();
        state.monzo.accounts.push(AccountLink {
            monzo_id: "acc_1".into(),
            actual_id: "u1".into(),
            ..Default::default()
        });
        state.monzo.accounts.push(AccountLink {
            monzo_id: "acc_2".into(),
            actual_id: "u2".into(),
            ..Default::default()
        });

        // An already-RESOLVED pending review (resolve_to an existing payee) whose
        // source account is unmapped. take_resolved will turn it into a
        // ResolvedReview routed by source_monzo_id = "acc_unmapped".
        state.pending_reviews.push(PendingReview {
            external_id: "tx_unmapped".into(),
            source_monzo_id: "acc_unmapped".into(),
            raw_merchant: "SQ *CAFE".into(),
            amount: -450,
            date: "2026-05-30".into(),
            resolve_to: "Local Cafe".into(), // exists in fake_actual
            create_new: false,
            suggestion: None,
            confidence: None,
            category_suggestion: None,
            category_confidence: None,
            resolve_category_to: String::new(),
            category_resolved: true,
            telegram_message_id: None,
        });

        let mut bank = FakeBank { txs: vec![] };
        let claude = FakeClaude { out: vec![] };
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

        // NOT imported (no routable account).
        assert!(
            !imports_log.exists(),
            "unmapped-source resolved review must NOT be imported"
        );
        // RE-HELD: the entry is back in pending_reviews under its external_id,
        // not dropped, so it survives until `aba accounts` maps the account.
        let reheld = state
            .pending_reviews
            .iter()
            .find(|p| p.external_id == "tx_unmapped")
            .expect("resolved-but-unmapped review must be re-held, not dropped");
        assert_eq!(
            reheld.source_monzo_id, "acc_unmapped",
            "source preserved for re-routing once mapped"
        );
        assert_eq!(
            reheld.suggestion.as_deref(),
            Some("Local Cafe"),
            "resolved payee preserved as the suggestion for next confirmation"
        );
        // And it persisted to disk (the run saved on success).
        let on_disk = State::load(&state_path).unwrap();
        assert!(
            on_disk
                .pending_reviews
                .iter()
                .any(|p| p.external_id == "tx_unmapped"),
            "re-hold must survive the successful run's save"
        );
    }

    #[test]
    fn suggest_links_matches_by_name_and_leaves_unmatched_unmapped() {
        let monzo = vec![
            ("acc_1".to_string(), "Personal".to_string()),
            ("acc_2".to_string(), "Joint Account".to_string()),
            ("acc_x".to_string(), "Nothing Like It".to_string()),
        ];
        let actual = vec![
            ("u1".to_string(), "Monzo Personal".to_string()),
            ("u2".to_string(), "Monzo Joint".to_string()),
        ];
        let links = suggest_links(&monzo, &actual, 0.6);
        let p = links.iter().find(|l| l.monzo_id == "acc_1").unwrap();
        assert_eq!(p.actual_id, "u1");
        let j = links.iter().find(|l| l.monzo_id == "acc_2").unwrap();
        assert_eq!(j.actual_id, "u2");
        let x = links.iter().find(|l| l.monzo_id == "acc_x").unwrap();
        assert_eq!(x.actual_id, "", "no confident match → unmapped");
    }

    #[test]
    fn schedules_from_rows_drops_bad_date_and_missing_payee_loudly() {
        let payees = vec![Payee {
            id: "p1".into(),
            name: "Rent Co".into(),
        }];
        let rows = vec![
            // good
            ScheduleRow {
                date: Some("2026-06-01".into()),
                amount: Some(-1000),
                payee_id: Some("p1".into()),
            },
            // bad date -> dropped
            ScheduleRow {
                date: Some("not-a-date".into()),
                amount: Some(-1000),
                payee_id: Some("p1".into()),
            },
            // missing date -> dropped
            ScheduleRow {
                date: None,
                amount: Some(-1000),
                payee_id: Some("p1".into()),
            },
            // payee not in list -> dropped
            ScheduleRow {
                date: Some("2026-06-02".into()),
                amount: Some(-1000),
                payee_id: Some("nope".into()),
            },
        ];
        let schedules = super::schedules_from_rows(rows, &payees);
        assert_eq!(schedules.len(), 1, "only the fully-valid row survives");
        assert_eq!(schedules[0].payee, "Rent Co");
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
