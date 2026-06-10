//! Run pipeline and auth bootstrap.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Context;
use chrono::Utc;
use tracing::{info, warn};

use crate::actual::ActualClient;
use crate::actual::shim_protocol::ImportTxn;
use crate::banks::{BankError, BankSource, RawTransaction, TransferTarget};
use crate::claude::{Classification, ClaudeMatcher, Confidence};
use crate::config::{Config, DATE_TOLERANCE_DAYS, PAYEE_THRESHOLD};
use crate::matching::matches_scheduled;
use crate::notify::Notifier;
#[cfg(test)]
use crate::state::{AccountLink, PendingReview, PotLink};
use crate::state::{ClassifiedHold, HeldTransfer, State};
use crate::telegram::ConfirmChannel;
use crate::telegram::prompts::{rehold_failed_remaps, send_group_prompts};

mod auth;
#[cfg(feature = "auth-flow")]
pub use auth::auth_monzo;
use auth::persist_rotated_token;
mod accounts;
pub use accounts::map_accounts;
mod classify;
use classify::{apply_matches, category_id_for, payee_id_for, plan_resolution};
mod transfer;
use transfer::{actual_id_for, resolve_transfer_dest, transfer_payee_id};
mod import;
use import::{hold_unmapped, schedules_from_rows};
// Re-exported (not just `use`d) because the top-level `report` module —
// a sibling of `pipeline`, not a descendant — needs
// `crate::pipeline::HeldClassification`, and `mod classify;` is private.
pub use classify::HeldClassification;

#[cfg(test)]
pub mod testutil;

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
    let started = std::time::Instant::now();
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
    // Partition transfers out of the ordinary pipeline BEFORE computing
    // `fetched_count`, so the scheduled-skip math stays about ordinary rows only.
    let (transfer_rows, fetched): (Vec<RawTransaction>, Vec<RawTransaction>) =
        fetched.into_iter().partition(|t| t.transfer.is_some());
    let fetched_count = fetched.len();

    // ---- Flex mirror handling (Phase 3, Flex) --------------------------------
    // A `monzo_flex` line on the Flex feed mirrors a main-side transfer. Suppress
    // it when its twin (same abs amount + date) is a main-side Account(flex)
    // transfer this run; otherwise hold it (fail-loud). Re-evaluate prior held
    // mirrors too, so a mirror held in an earlier run resolves once its twin arrives.
    let flex_account_id: Option<String> = state
        .monzo
        .accounts
        .iter()
        .find(|a| a.is_flex)
        .map(|a| a.monzo_id.clone());

    // Keys of main-side Flex transfers THIS run: (abs amount, "YYYY-MM-DD").
    // (abs amount, date) is imprecise for same-amount same-day duplicates: it
    // suppresses rather than double-imports (the main side fully represents the
    // movement), so this is the safe direction. Rare in practice.
    let main_flex_keys: std::collections::BTreeSet<(i64, String)> = transfer_rows
        .iter()
        .filter(|t| match (&t.transfer, &flex_account_id) {
            (Some(TransferTarget::Account(dest)), Some(fid)) => dest == fid,
            _ => false,
        })
        .map(|t| (t.amount_minor.abs(), t.date.format("%Y-%m-%d").to_string()))
        .collect();

    // Split mirrors out so the Account/Pot intent loop below never sees them.
    let (mirror_rows, transfer_rows): (Vec<RawTransaction>, Vec<RawTransaction>) = transfer_rows
        .into_iter()
        .partition(|t| matches!(t.transfer, Some(TransferTarget::FlexMirror)));

    // Pull prior held mirrors out of the durable queue; we re-decide their fate.
    let prior_held_mirrors: Vec<HeldTransfer> = state
        .held_transfers
        .iter()
        .filter(|h| h.target_kind == "flex_mirror")
        .cloned()
        .collect();
    state
        .held_transfers
        .retain(|h| h.target_kind != "flex_mirror");

    let mut flex_mirrors_suppressed = 0usize;
    // from_row is infallible here: every mirror_rows entry carries Some(FlexMirror).
    let new_mirror_hts = mirror_rows.iter().filter_map(HeldTransfer::from_row);
    for ht in prior_held_mirrors.into_iter().chain(new_mirror_hts) {
        let key = (ht.amount.abs(), ht.date.clone());
        if main_flex_keys.contains(&key) {
            flex_mirrors_suppressed += 1; // paired -> drop (Actual recreates this side)
        } else {
            state.enqueue_held_transfer(ht); // unpaired -> hold (dedup by external_id)
        }
    }

    // 3. Read Actual payees + schedules + categories. `get_payees` must stay
    // strictly before `get_categories`: the rotated-token regression test injects
    // a `get_payees` failure and asserts the run aborts at that earlier point.
    let payees = actual.get_payees().await.context("get_payees")?;
    let schedule_rows = actual.get_scheduled().await.context("get_scheduled")?;
    let schedules = schedules_from_rows(schedule_rows, &payees);
    let categories = actual.get_categories().await.context("get_categories")?;
    let category_names: Vec<String> = categories.iter().map(|c| c.name.clone()).collect();
    // Zero categories => degrade to Phase-1 behavior (category dimension auto-
    // settles to uncategorised everywhere, never prompts).
    let categories_available = !category_names.is_empty();

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
    // resolve_to cleared, prompt re-sent next run). Edits the stale inline-keyboard
    // message once per group (shared message_id) before re-holding.
    rehold_failed_remaps(state, channel, &payee_names).await;

    // 5a. Apply already-resolved pending reviews from previous runs.
    let resolved = state.take_resolved();

    // 5b. Cache split + Claude for the rest. A FULL cache hit needs both the
    // payee AND the category cached; a legacy payee-only entry falls through to
    // Claude (and the gating path) so its category gets resolved.
    let plan = plan_resolution(&candidates, &state.payee_cache, &state.category_cache);
    let claude_matches: Vec<Classification> = if plan.needs_claude.is_empty() {
        Vec::new()
    } else {
        claude
            .classify(&plan.needs_claude, &payee_names, &category_names)
            .await
            .context("claude classify")?
    };
    // Everything that was NOT a full cache hit is gated by apply_matches.
    let cached_ids: BTreeSet<&str> = plan.cached.iter().map(|(id, _, _)| id.as_str()).collect();
    let unknown_txs: Vec<RawTransaction> = candidates
        .iter()
        .filter(|tx| !cached_ids.contains(tx.external_id.as_str()))
        .cloned()
        .collect();
    let existing_payees: BTreeSet<&str> = payee_names.iter().map(String::as_str).collect();
    let existing_cats: BTreeSet<&str> = category_names.iter().map(String::as_str).collect();
    let outcome = apply_matches(
        &unknown_txs,
        &claude_matches,
        autoimport,
        &existing_payees,
        &existing_cats,
        categories_available,
    );

    // Clone links so the routing closure/holds don't fight the borrow checker
    // against `state` mutations below. Cheap: a handful of entries.
    let account_links = state.monzo.accounts.clone();

    // 6. Assemble import rows (payee NAME -> id; never auto-create on the matched path).
    let mut grouped: BTreeMap<String, Vec<ImportTxn>> = BTreeMap::new();
    let mut held = 0usize;

    // ---- Phase 3: transfers (retry queue + newly fetched) ----
    // Transfers bypass the Claude/payee/category pipeline: resolve dest -> Actual
    // account -> that account's transfer payee, and import ONE uncategorised row
    // into the SOURCE account's group (Actual auto-creates the linked counterpart).
    let pot_links = state.monzo.pots.clone();
    // Work list: previously-held transfers (retry) + newly fetched, deduped by external_id.
    // NOTE: flex_mirror HeldTransfers enqueued by the Flex mirror block above are
    // deliberately re-cloned here and re-held via FlexMirror -> resolve_transfer_dest
    // None -> still_held. Do NOT move the mirror block below this clone.
    let mut transfer_intents: Vec<HeldTransfer> = state.held_transfers.clone();
    for row in &transfer_rows {
        if let Some(ht) = HeldTransfer::from_row(row)
            && !transfer_intents
                .iter()
                .any(|h| h.external_id == ht.external_id)
        {
            transfer_intents.push(ht);
        }
    }

    let mut still_held: Vec<HeldTransfer> = Vec::new();
    let mut transfers_imported = 0usize;
    for ht in transfer_intents {
        let Some(target) = ht.target() else {
            warn!(
                "held transfer {} has unknown target_kind '{}'; re-holding (state corruption?)",
                ht.external_id, ht.target_kind
            );
            still_held.push(ht);
            continue;
        };
        let dest = resolve_transfer_dest(&target, &pot_links, &account_links);
        let src = actual_id_for(&ht.source_monzo_id, &account_links);
        match (src, dest) {
            (Some(src_actual), Some(dest_actual)) => {
                if let Some(pid) = transfer_payee_id(&payees, &dest_actual) {
                    grouped.entry(src_actual).or_default().push(ImportTxn {
                        date: ht.date.clone(),
                        amount: ht.amount,
                        payee: pid.to_string(),
                        category: None, // transfers are uncategorised
                        imported_id: ht.external_id.clone(),
                    });
                    transfers_imported += 1;
                } else {
                    warn!(
                        "no transfer payee for account {dest_actual}; re-holding transfer {}",
                        ht.external_id
                    );
                    still_held.push(ht);
                }
            }
            _ => still_held.push(ht), // source and/or dest unmapped -> keep, retry next run
        }
    }
    state.held_transfers = still_held;

    let make_row = |tx: &RawTransaction, payee_id: &str, category_id: Option<String>| ImportTxn {
        date: tx.date.format("%Y-%m-%d").to_string(),
        amount: tx.amount_minor,
        payee: payee_id.to_string(),
        category: category_id,
        imported_id: tx.external_id.clone(),
    };

    // Resolve a cached/matched category NAME (empty = uncategorised) to a row
    // `category` value. `Some(_)` = resolved (inner `None` = uncategorised);
    // outer `None` = the name no longer resolves -> caller must hold (fail-loud).
    let resolve_cat = |category_name: &str| -> Option<Option<String>> {
        if category_name.is_empty() {
            Some(None) // uncategorised, valid
        } else {
            category_id_for(&categories, category_name).map(|id| Some(id.to_string()))
        }
    };

    let by_id: BTreeMap<&str, &RawTransaction> = candidates
        .iter()
        .map(|t| (t.external_id.as_str(), t))
        .collect();

    // cached hits (full: payee + category)
    for (ext_id, payee_name, category_name) in &plan.cached {
        let cat_id = resolve_cat(category_name);
        match (
            by_id.get(ext_id.as_str()),
            payee_id_for(&payees, payee_name),
        ) {
            (Some(tx), Some(pid)) => {
                if let Some(cat) = &cat_id {
                    if let Some(aid) = actual_id_for(&tx.source_monzo_id, &account_links) {
                        grouped
                            .entry(aid)
                            .or_default()
                            .push(make_row(tx, pid, cat.clone()));
                    } else {
                        warn!(
                            "tx {ext_id} from unmapped Monzo account '{}' — holding (run `aba accounts`)",
                            tx.source_monzo_id
                        );
                        hold_unmapped(state, tx, &mut held);
                    }
                } else {
                    // Category stale, payee OK -> pre-settle payee, re-prompt
                    // category (fail-loud: never silently import uncategorised).
                    warn!(
                        "cached category '{category_name}' for tx {ext_id} no longer exists in Actual; re-prompting category"
                    );
                    let date = tx.date.format("%Y-%m-%d").to_string();
                    if state.add_pending_classified(
                        &tx.external_id,
                        &tx.raw_merchant,
                        tx.amount_minor,
                        &date,
                        &tx.source_monzo_id,
                        ClassifiedHold {
                            payee_resolved: Some(payee_name.clone()),
                            // Stale name; off-list -> build_prompt offers free-remap/Skip.
                            category_suggestion: Some(category_name.clone()),
                            ..Default::default()
                        },
                    ) {
                        held += 1;
                    }
                }
            }
            (Some(tx), None) => {
                // Payee stale -> re-prompt payee; preserve the category if it
                // still resolves, else re-prompt it too. `cat_id` is the
                // resolved value: Some(_) resolves (an id, or empty=uncategorised
                // sentinel) -> pre-settle to the cached name; None -> also stale.
                warn!(
                    "cached payee '{payee_name}' for tx {ext_id} no longer exists in Actual; re-prompting payee"
                );
                let (category_resolved, category_suggestion) = match &cat_id {
                    Some(_) => (Some(category_name.clone()), None),
                    None => (None, Some(category_name.clone())),
                };
                let date = tx.date.format("%Y-%m-%d").to_string();
                if state.add_pending_classified(
                    &tx.external_id,
                    &tx.raw_merchant,
                    tx.amount_minor,
                    &date,
                    &tx.source_monzo_id,
                    ClassifiedHold {
                        // Stale payee -> "create it?" flavor on the next prompt.
                        payee_suggestion: Some(payee_name.clone()),
                        category_resolved,
                        category_suggestion,
                        ..Default::default()
                    },
                ) {
                    held += 1;
                }
            }
            _ => {}
        }
    }
    // claude matches (both dimensions confident)
    let mut newly_cached: Vec<(String, String)> = Vec::new();
    for (ext_id, payee_name, category_name) in &outcome.matched {
        let cat_id = resolve_cat(category_name);
        match (
            by_id.get(ext_id.as_str()),
            payee_id_for(&payees, payee_name),
        ) {
            (Some(tx), Some(pid)) if cat_id.is_some() => {
                let cat = cat_id.unwrap();
                if let Some(aid) = actual_id_for(&tx.source_monzo_id, &account_links) {
                    grouped.entry(aid).or_default().push(make_row(tx, pid, cat));
                    if !state.payee_cache.contains_key(&tx.raw_merchant) {
                        state
                            .payee_cache
                            .insert(tx.raw_merchant.clone(), payee_name.clone());
                        newly_cached.push((tx.raw_merchant.clone(), payee_name.clone()));
                    }
                    // Only cache a real (non-uncategorised) category.
                    if !category_name.is_empty() {
                        state
                            .category_cache
                            .insert(tx.raw_merchant.clone(), category_name.clone());
                    }
                } else {
                    warn!(
                        "matched tx {ext_id} from unmapped Monzo account '{}' — holding (run `aba accounts`)",
                        tx.source_monzo_id
                    );
                    hold_unmapped(state, tx, &mut held);
                }
            }
            (Some(tx), payee_id) => {
                // Defensive: apply_matches guarantees matched payee+category
                // exist. If this ever fires (a concurrent delete in Actual), hold
                // rather than drop, re-prompting only the stale dimension(s).
                warn!(
                    "matched payee/category for tx {ext_id} unexpectedly missing; holding for review"
                );
                // Payee resolves -> pre-settle it; else suggest it for review.
                let (payee_resolved, payee_suggestion) = if payee_id.is_some() {
                    (Some(payee_name.clone()), None)
                } else {
                    (None, Some(payee_name.clone()))
                };
                // Category resolves (or empty sentinel) -> pre-settle; else suggest.
                let (category_resolved, category_suggestion) = match &cat_id {
                    Some(_) => (Some(category_name.clone()), None),
                    None => (None, Some(category_name.clone())),
                };
                let date = tx.date.format("%Y-%m-%d").to_string();
                if state.add_pending_classified(
                    &tx.external_id,
                    &tx.raw_merchant,
                    tx.amount_minor,
                    &date,
                    &tx.source_monzo_id,
                    ClassifiedHold {
                        payee_resolved,
                        payee_suggestion,
                        category_resolved,
                        category_suggestion,
                        ..Default::default()
                    },
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
        // Resolve the category name to an id; empty/None = uncategorised.
        let cat_id: Option<String> = match &r.category_name {
            None => None,
            Some(name) => {
                if let Some(id) = category_id_for(&categories, name) {
                    Some(id.to_string())
                } else {
                    // Fail-loud: resolved category no longer exists -> pre-settle
                    // the payee, re-prompt ONLY the category (never collapse it to
                    // uncategorised, and never re-ask the already-resolved payee).
                    warn!(
                        "resolved category '{name}' for '{}' no longer exists; re-prompting category",
                        r.raw_merchant
                    );
                    if state.add_pending_classified(
                        &r.external_id,
                        &r.raw_merchant,
                        r.amount,
                        &r.date,
                        &r.source_monzo_id,
                        ClassifiedHold {
                            payee_resolved: Some(r.payee_name.clone()),
                            category_suggestion: Some(name.clone()),
                            ..Default::default()
                        },
                    ) {
                        held += 1;
                    }
                    continue;
                }
            }
        };
        if let Some(aid) = actual_id_for(&r.source_monzo_id, &account_links) {
            grouped.entry(aid).or_default().push(ImportTxn {
                date: r.date.clone(),
                amount: r.amount,
                payee: pid,
                category: cat_id,
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

    // held classifications -> pending reviews carrying both dimensions' state.
    for h in &outcome.held {
        let date = h.tx.date.format("%Y-%m-%d").to_string();
        let added = state.add_pending_classified(
            &h.tx.external_id,
            &h.tx.raw_merchant,
            h.tx.amount_minor,
            &date,
            &h.tx.source_monzo_id,
            ClassifiedHold {
                payee_resolved: h.payee_resolved.clone(),
                payee_suggestion: h.payee_suggestion.clone(),
                payee_confidence: h.payee_confidence.map(|c| c.as_str().to_string()),
                category_resolved: h.category_resolved.clone(),
                category_suggestion: h.category_suggestion.clone(),
                category_confidence: h.category_confidence.map(|c| c.as_str().to_string()),
            },
        );
        if added {
            held += 1;
        }
    }

    // 7. Import or report.
    if dry_run {
        crate::report::dry_run(
            &grouped,
            &outcome.held,
            &newly_cached,
            &crate::report::DryRunCounts {
                would_skip: fetched_count.saturating_sub(candidates.len()),
                held,
                transfers_imported,
                transfers_held: state.held_transfers.len(),
                flex_mirrors_suppressed,
            },
        );
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
    state.last_run = Some(fetch_start);

    // Send one prompt per merchant group for everything still held.
    if let Some(ch) = channel {
        send_group_prompts(state, ch, &payee_names, &category_names, &cfg.state_path).await;
    }

    state.save(&cfg.state_path)?;

    let awaiting = state
        .pending_reviews
        .iter()
        .map(|p| p.raw_merchant.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    info!(
        fetched = fetched_count,
        added = summary.added,
        updated = summary.updated,
        held,
        transfers_imported,
        flex_mirrors_suppressed,
        awaiting_merchants = awaiting,
        held_transfers = state.held_transfers.len(),
        duration_secs = started.elapsed().as_secs(),
        "run complete"
    );
    let body = crate::report::summary_body(
        summary.added,
        summary.updated,
        held,
        &newly_cached,
        awaiting,
        transfers_imported,
        state.held_transfers.len(),
    );
    notifier.notify("Actual import complete", &body).await.ok();

    Ok(())
}

#[cfg(test)]
pub mod tests {
    use super::testutil::*;
    use super::*;
    use chrono::NaiveDate;

    #[test]
    fn awaiting_count_is_per_merchant() {
        use std::collections::BTreeSet;
        let reviews = [
            held("tx1", "TFL TRAVEL CH"),
            held("tx2", "TFL TRAVEL CH"),
            held("tx3", "TESCO 1234"),
        ];
        let merchants = reviews
            .iter()
            .map(|p| p.raw_merchant.as_str())
            .collect::<BTreeSet<_>>()
            .len();
        assert_eq!(merchants, 2, "three txns across two merchants");
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
            out: vec![Classification {
                raw: "SQ *CAFE".into(),
                payee: Some("Local Cafe".into()),
                payee_confidence: Confidence::Medium,
                // Confident, existing category -> category dimension PRE-SETTLED;
                // only the payee is reviewed (one prompt sent, no category prompt).
                category: Some("Dining".into()),
                category_confidence: Confidence::High,
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
            transfer: None,
        };
        let mut bank = FakeBank {
            txs: vec![tx_cafe, tx_bills],
        };
        let claude = FakeClaude {
            out: vec![
                Classification {
                    raw: "SQ *CAFE".into(),
                    payee: Some("Local Cafe".into()),
                    payee_confidence: Confidence::High,
                    // Confident, existing category -> both dimensions confident,
                    // so the tx still auto-imports (now with a category).
                    category: Some("Groceries".into()),
                    category_confidence: Confidence::High,
                },
                Classification {
                    raw: "BILLS LTD".into(),
                    payee: Some("Bills Co".into()),
                    payee_confidence: Confidence::High,
                    category: Some("Groceries".into()),
                    category_confidence: Confidence::High,
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
    async fn pot_deposit_imports_as_transfer_when_mapped() {
        let shim = tempfile::tempdir().unwrap();
        let statedir = tempfile::tempdir().unwrap();
        let state_path = statedir.path().join("s.toml");
        let routed_log = shim.path().join("routed.log");
        let cfg = cfg_at(&state_path);
        let actual = fake_actual_transfers(&shim);
        let notifier = NullNotifier;

        let mut state = State::default();
        // Source account mapped: acc_cur -> act-cur.
        state.monzo.accounts.push(AccountLink {
            monzo_id: "acc_cur".into(),
            actual_id: "act-cur".into(),
            ..Default::default()
        });
        // Pot mapped: pot_abc -> act-pot.
        state.monzo.pots.push(PotLink {
            monzo_pot_id: "pot_abc".into(),
            actual_id: "act-pot".into(),
            ..Default::default()
        });

        let mut bank = FakeBank {
            txs: vec![pot_deposit("xfer_1")],
        };
        let claude = FakeClaude { out: vec![] }; // transfers bypass Claude
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

        let logged = std::fs::read_to_string(&routed_log).unwrap();
        let calls: Vec<serde_json::Value> = logged
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(calls.len(), 1, "exactly one transfer import");
        assert_eq!(
            calls[0]["account_id"], "act-cur",
            "routed to SOURCE account"
        );
        let txns = calls[0]["transactions"].as_array().unwrap();
        assert_eq!(txns.len(), 1);
        assert_eq!(
            txns[0]["payee"], "p_xfer_pot",
            "imported under the destination account's transfer payee"
        );
        assert!(
            txns[0].get("category").is_none(),
            "transfers are uncategorised -> no category field on the wire"
        );
        assert!(
            state.held_transfers.is_empty(),
            "mapped transfer must not be held"
        );
    }

    #[tokio::test]
    async fn pot_deposit_is_held_when_pot_unmapped() {
        let shim = tempfile::tempdir().unwrap();
        let statedir = tempfile::tempdir().unwrap();
        let state_path = statedir.path().join("s.toml");
        let routed_log = shim.path().join("routed.log");
        let cfg = cfg_at(&state_path);
        let actual = fake_actual_transfers(&shim);
        let notifier = NullNotifier;

        let mut state = State::default();
        // Source mapped, but the pot is NOT mapped (pots empty).
        state.monzo.accounts.push(AccountLink {
            monzo_id: "acc_cur".into(),
            actual_id: "act-cur".into(),
            ..Default::default()
        });

        let mut bank = FakeBank {
            txs: vec![pot_deposit("xfer_1")],
        };
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

        // The transfer was NOT imported: routed.log either absent or has no
        // transfer import.
        if routed_log.exists() {
            let logged = std::fs::read_to_string(&routed_log).unwrap();
            assert!(
                !logged.contains("p_xfer_pot"),
                "unmapped pot must NOT be imported, got: {logged}"
            );
        }
        assert_eq!(state.held_transfers.len(), 1, "unmapped pot -> held");
        let ht = &state.held_transfers[0];
        assert_eq!(ht.target_kind, "pot");
        assert_eq!(ht.target_id, "pot_abc");
        assert_eq!(ht.source_monzo_id, "acc_cur");
        assert!(
            state.pending_reviews.is_empty(),
            "transfers do NOT use the Telegram review path"
        );
    }

    /// Held transfer is retried and imported once the pot is mapped on a
    /// subsequent run.  The second `run()` uses an empty `FakeBank` so the import
    /// can ONLY originate from the held queue, proving durable retry works.
    #[tokio::test]
    async fn held_transfer_imports_after_pot_is_mapped() {
        let shim = tempfile::tempdir().unwrap();
        let statedir = tempfile::tempdir().unwrap();
        let state_path = statedir.path().join("s.toml");
        let routed_log = shim.path().join("routed.log");
        let cfg = cfg_at(&state_path);
        let actual = fake_actual_transfers(&shim);
        let notifier = NullNotifier;

        // Source account mapped; pot NOT yet mapped.
        let mut state = State::default();
        state.monzo.accounts.push(AccountLink {
            monzo_id: "acc_cur".into(),
            actual_id: "act-cur".into(),
            ..Default::default()
        });

        // Run 1: fetch the pot-deposit tx.  Pot is unmapped -> held.
        let mut bank1 = FakeBank {
            txs: vec![pot_deposit("tx_pot")],
        };
        let claude = FakeClaude { out: vec![] };
        run(
            &cfg,
            false,
            &mut state,
            &mut bank1,
            &actual,
            &claude,
            &notifier,
            None,
            Confidence::High,
        )
        .await
        .unwrap();

        assert_eq!(
            state.held_transfers.len(),
            1,
            "run 1 must hold the transfer"
        );
        // routed.log must not contain a transfer import yet.
        if routed_log.exists() {
            let logged = std::fs::read_to_string(&routed_log).unwrap();
            assert!(
                !logged.contains("p_xfer_pot"),
                "no transfer import before pot is mapped, got: {logged}"
            );
        }

        // Now map the pot.
        state.monzo.pots.push(PotLink {
            monzo_pot_id: "pot_abc".into(),
            actual_id: "act-pot".into(),
            ..Default::default()
        });

        // Run 2: empty bank (no new fetch rows) -> held entry must be retried.
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
            None,
            Confidence::High,
        )
        .await
        .unwrap();

        assert!(
            state.held_transfers.is_empty(),
            "after mapping the pot the held transfer must be cleared"
        );
        // file must exist: an absent log means the retry never fired
        let logged = std::fs::read_to_string(&routed_log)
            .expect("routed.log must exist — retry path never fired");
        let calls: Vec<serde_json::Value> = logged
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(calls.len(), 1, "exactly one import call on run 2");
        assert_eq!(
            calls[0]["account_id"], "act-cur",
            "import routed to source account"
        );
        let txns = calls[0]["transactions"].as_array().unwrap();
        assert_eq!(txns.len(), 1);
        assert_eq!(
            txns[0]["payee"], "p_xfer_pot",
            "payee must be the pot transfer payee"
        );
        assert!(
            txns[0].get("category").is_none(),
            "transfers carry no category"
        );
    }

    /// A re-fetched duplicate of a held transfer (same `external_id`) must NOT
    /// double-queue the entry.  After two runs with the same unmapped tx the
    /// held list must still have length 1.
    #[tokio::test]
    async fn held_transfer_not_double_queued_when_refetched() {
        let shim = tempfile::tempdir().unwrap();
        let statedir = tempfile::tempdir().unwrap();
        let state_path = statedir.path().join("s.toml");
        let routed_log = shim.path().join("routed.log");
        let cfg = cfg_at(&state_path);
        let actual = fake_actual_transfers(&shim);
        let notifier = NullNotifier;

        // Source mapped; pot stays unmapped for the whole test.
        let mut state = State::default();
        state.monzo.accounts.push(AccountLink {
            monzo_id: "acc_cur".into(),
            actual_id: "act-cur".into(),
            ..Default::default()
        });

        // Run 1: pot-deposit tx fetched, unmapped -> held (len == 1).
        let mut bank1 = FakeBank {
            txs: vec![pot_deposit("tx_pot")],
        };
        let claude = FakeClaude { out: vec![] };
        run(
            &cfg,
            false,
            &mut state,
            &mut bank1,
            &actual,
            &claude,
            &notifier,
            None,
            Confidence::High,
        )
        .await
        .unwrap();

        assert_eq!(state.held_transfers.len(), 1, "run 1: tx held");

        // Run 2: the overlap window re-fetches the SAME tx again (still unmapped).
        let mut bank2 = FakeBank {
            txs: vec![pot_deposit("tx_pot")],
        };
        let claude2 = FakeClaude { out: vec![] };
        run(
            &cfg,
            false,
            &mut state,
            &mut bank2,
            &actual,
            &claude2,
            &notifier,
            None,
            Confidence::High,
        )
        .await
        .unwrap();

        assert_eq!(
            state.held_transfers.len(),
            1,
            "duplicate external_id must not double-queue held transfers"
        );
        // Still no import.
        if routed_log.exists() {
            let logged = std::fs::read_to_string(&routed_log).unwrap();
            assert!(
                !logged.contains("p_xfer_pot"),
                "unmapped pot must never import, got: {logged}"
            );
        }
    }

    #[tokio::test]
    async fn flex_repayment_imports_main_side_and_suppresses_mirror() {
        // accounts: acc_cur->act-cur (main), acc_flex->act-flex (is_flex=true)
        let shim = tempfile::tempdir().unwrap();
        let statedir = tempfile::tempdir().unwrap();
        let state_path = statedir.path().join("s.toml");
        let routed_log = shim.path().join("routed.log");
        let cfg = cfg_at(&state_path);
        let actual = fake_actual_transfers(&shim);
        let notifier = NullNotifier;

        let mut state = State::default();
        state.monzo.accounts.push(AccountLink {
            monzo_id: "acc_cur".into(),
            actual_id: "act-cur".into(),
            ..Default::default()
        });
        state.monzo.accounts.push(AccountLink {
            monzo_id: "acc_flex".into(),
            actual_id: "act-flex".into(),
            is_flex: true,
            ..Default::default()
        });

        // Both sides of one repayment, same abs amount + date:
        let main = RawTransaction {
            external_id: "tx_main".into(),
            date: NaiveDate::from_ymd_opt(2026, 6, 1).unwrap(),
            amount_minor: -82822,
            raw_merchant: "Flex".into(),
            source_monzo_id: "acc_cur".into(),
            transfer: Some(TransferTarget::Account("acc_flex".into())),
        };
        let mirror = RawTransaction {
            external_id: "tx_mirror".into(),
            date: NaiveDate::from_ymd_opt(2026, 6, 1).unwrap(),
            amount_minor: 82822,
            raw_merchant: "Flex".into(),
            source_monzo_id: "acc_flex".into(),
            transfer: Some(TransferTarget::FlexMirror),
        };
        let mut bank = FakeBank {
            txs: vec![main, mirror],
        };
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

        let logged = std::fs::read_to_string(&routed_log).expect("routed.log must exist");
        let lines: Vec<&str> = logged.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 1, "exactly one import call (main side only)");
        assert!(
            lines[0].contains("act-cur"),
            "imported into the main account"
        );
        assert!(
            lines[0].contains("p_xfer_flex"),
            "via the Flex transfer payee"
        );
        assert!(
            state.held_transfers.is_empty(),
            "mirror suppressed, nothing held"
        );
    }

    #[tokio::test]
    async fn unpaired_flex_mirror_is_held_not_dropped() {
        let shim = tempfile::tempdir().unwrap();
        let statedir = tempfile::tempdir().unwrap();
        let state_path = statedir.path().join("s.toml");
        let routed_log = shim.path().join("routed.log");
        let cfg = cfg_at(&state_path);
        let actual = fake_actual_transfers(&shim);
        let notifier = NullNotifier;

        let mut state = State::default();
        state.monzo.accounts.push(AccountLink {
            monzo_id: "acc_cur".into(),
            actual_id: "act-cur".into(),
            ..Default::default()
        });
        state.monzo.accounts.push(AccountLink {
            monzo_id: "acc_flex".into(),
            actual_id: "act-flex".into(),
            is_flex: true,
            ..Default::default()
        });

        // ONLY a flex-side mirror, no main twin this run:
        let mirror = RawTransaction {
            external_id: "tx_lonely".into(),
            date: NaiveDate::from_ymd_opt(2026, 6, 1).unwrap(),
            amount_minor: 500,
            raw_merchant: "Flex".into(),
            source_monzo_id: "acc_flex".into(),
            transfer: Some(TransferTarget::FlexMirror),
        };
        let mut bank = FakeBank { txs: vec![mirror] };
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

        // no pot import; held with target_kind flex_mirror; not a Telegram review
        let imported = std::fs::read_to_string(&routed_log)
            .map_or(0, |s| s.lines().filter(|l| !l.trim().is_empty()).count());
        assert_eq!(imported, 0, "unpaired mirror not imported");
        assert_eq!(state.held_transfers.len(), 1, "unpaired mirror held");
        assert_eq!(state.held_transfers[0].target_kind, "flex_mirror");
        assert!(
            state.pending_reviews.is_empty(),
            "transfers never use the Telegram review path"
        );
    }

    #[tokio::test]
    async fn held_flex_mirror_suppressed_when_twin_arrives_next_run() {
        // A mirror held in run 1 must be SUPPRESSED in run 2 once its main-side
        // twin (same abs amount + date) finally arrives — the only F5 path the
        // two single-run tests don't exercise.
        let shim = tempfile::tempdir().unwrap();
        let statedir = tempfile::tempdir().unwrap();
        let state_path = statedir.path().join("s.toml");
        let routed_log = shim.path().join("routed.log");
        let cfg = cfg_at(&state_path);
        let actual = fake_actual_transfers(&shim);
        let notifier = NullNotifier;
        let date = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();

        let mut state = State::default();
        state.monzo.accounts.push(AccountLink {
            monzo_id: "acc_cur".into(),
            actual_id: "act-cur".into(),
            ..Default::default()
        });
        state.monzo.accounts.push(AccountLink {
            monzo_id: "acc_flex".into(),
            actual_id: "act-flex".into(),
            is_flex: true,
            ..Default::default()
        });

        // Run 1: only the flex-side mirror -> held.
        let mirror = RawTransaction {
            external_id: "tx_m".into(),
            date,
            amount_minor: 500,
            raw_merchant: "Flex".into(),
            source_monzo_id: "acc_flex".into(),
            transfer: Some(TransferTarget::FlexMirror),
        };
        let mut bank1 = FakeBank { txs: vec![mirror] };
        let claude = FakeClaude { out: vec![] };
        run(
            &cfg,
            false,
            &mut state,
            &mut bank1,
            &actual,
            &claude,
            &notifier,
            None,
            Confidence::High,
        )
        .await
        .unwrap();
        assert_eq!(state.held_transfers.len(), 1, "run 1: unpaired mirror held");
        assert_eq!(state.held_transfers[0].target_kind, "flex_mirror");

        // Run 2: a FRESH bank returns ONLY the main-side twin (same date + abs amount).
        let main = RawTransaction {
            external_id: "tx_main".into(),
            date,
            amount_minor: -500,
            raw_merchant: "Flex".into(),
            source_monzo_id: "acc_cur".into(),
            transfer: Some(TransferTarget::Account("acc_flex".into())),
        };
        let mut bank2 = FakeBank { txs: vec![main] };
        run(
            &cfg,
            false,
            &mut state,
            &mut bank2,
            &actual,
            &claude,
            &notifier,
            None,
            Confidence::High,
        )
        .await
        .unwrap();

        // The previously-held mirror is now suppressed; exactly one main-side import.
        assert!(
            state.held_transfers.is_empty(),
            "run 2: twin arrival suppresses the held mirror"
        );
        let logged = std::fs::read_to_string(&routed_log).expect("routed.log must exist");
        let lines: Vec<&str> = logged.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 1, "exactly one import call (main side only)");
        assert!(
            lines[0].contains("act-cur"),
            "imported into the main account"
        );
        assert!(
            lines[0].contains("p_xfer_flex"),
            "via the Flex transfer payee"
        );
    }

    #[tokio::test]
    async fn flex_flagged_but_unmapped_holds_main_and_suppresses_mirror() {
        // acc_cur -> act-cur (mapped). acc_flex is FLAGGED is_flex but has NO actual_id
        // (flagged-as-flex, not yet mapped to an Actual account).
        let shim = tempfile::tempdir().unwrap();
        let statedir = tempfile::tempdir().unwrap();
        let state_path = statedir.path().join("s.toml");
        let routed_log = shim.path().join("routed.log");
        let cfg = cfg_at(&state_path);
        let actual = fake_actual_transfers(&shim);
        let notifier = NullNotifier;

        let mut state = State::default();
        state.monzo.accounts.push(AccountLink {
            monzo_id: "acc_cur".into(),
            actual_id: "act-cur".into(),
            ..Default::default()
        });
        // is_flex = true but actual_id empty (unmapped):
        state.monzo.accounts.push(AccountLink {
            monzo_id: "acc_flex".into(),
            is_flex: true,
            ..Default::default()
        });

        // Repayment: both sides, same date + abs amount.
        let main = RawTransaction {
            external_id: "tx_main".into(),
            date: NaiveDate::from_ymd_opt(2026, 6, 1).unwrap(),
            amount_minor: -82822,
            raw_merchant: "Flex".into(),
            source_monzo_id: "acc_cur".into(),
            transfer: Some(TransferTarget::Account("acc_flex".into())),
        };
        let mirror = RawTransaction {
            external_id: "tx_mirror".into(),
            date: NaiveDate::from_ymd_opt(2026, 6, 1).unwrap(),
            amount_minor: 82822,
            raw_merchant: "Flex".into(),
            source_monzo_id: "acc_flex".into(),
            transfer: Some(TransferTarget::FlexMirror),
        };
        let mut bank = FakeBank {
            txs: vec![main, mirror],
        };
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

        // Nothing imported (Flex unmapped -> main side can't resolve a dest).
        let imported = std::fs::read_to_string(&routed_log)
            .map_or(0, |s| s.lines().filter(|l| !l.trim().is_empty()).count());
        assert_eq!(imported, 0, "nothing imported while Flex is unmapped");

        // Movement preserved: exactly ONE held entry — the MAIN side (target_kind "account"),
        // NOT the mirror (which was suppressed by amount+date pairing keyed on monzo_id).
        assert_eq!(state.held_transfers.len(), 1, "only the main side is held");
        assert_eq!(
            state.held_transfers[0].target_kind, "account",
            "held entry is the main transfer, not the mirror"
        );
        assert!(
            state
                .held_transfers
                .iter()
                .all(|h| h.target_kind != "flex_mirror"),
            "mirror was suppressed, not held"
        );
        assert!(state.pending_reviews.is_empty());
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
            out: vec![Classification {
                raw: "SQ *CAFE".into(),
                payee: Some("Local Cafe".into()),
                payee_confidence: Confidence::Medium,
                category: None,
                category_confidence: Confidence::Low,
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
            out: vec![Classification {
                raw: "SQ *CAFE".into(),
                payee: Some("Local Cafe".into()),
                payee_confidence: Confidence::Medium,
                category: None,
                category_confidence: Confidence::Low,
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
                pots: Vec::new(),
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
                pots: Vec::new(),
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
                pots: Vec::new(),
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
            out: vec![Classification {
                raw: "SQ *GHOST".into(),
                payee: Some("Ghost Payee".into()),
                payee_confidence: Confidence::High,
                category: None,
                category_confidence: Confidence::Low,
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
    async fn stale_cached_category_reprompts_category_not_payee() {
        // Fail-loud regression: a merchant cached with a payee that STILL exists
        // but a category that no longer does must NOT silently import
        // uncategorised, and must NOT re-ask the (still-valid) payee. It must
        // pre-settle the payee and re-prompt ONLY the category.
        let shim = tempfile::tempdir().unwrap();
        let statedir = tempfile::tempdir().unwrap();
        let state_path = statedir.path().join("s.toml");
        let imports_log = shim.path().join("imports.log");
        let cfg = cfg_at(&state_path);
        // fake_actual: payees [Local Cafe]; categories [Groceries, Dining].
        let actual = fake_actual(&shim);
        let notifier = NullNotifier;
        let channel = FakeChannel::new(vec![], 0);

        let mut state = State::default();
        state.monzo.accounts.push(AccountLink {
            monzo_id: "acc_1".into(),
            actual_id: "acc".into(),
            ..Default::default()
        });
        // Cached: payee exists in the shim; category "Utilities" does NOT.
        state
            .payee_cache
            .insert("SQ *CAFE".into(), "Local Cafe".into());
        state
            .category_cache
            .insert("SQ *CAFE".into(), "Utilities".into());

        // The tx is a full cache hit -> no Claude classification needed.
        let mut bank = FakeBank {
            txs: vec![tx("tx_cafe", "SQ *CAFE", -450)],
        };
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

        // Nothing imported (must not collapse to uncategorised).
        assert!(
            !imports_log.exists(),
            "stale category must NOT import uncategorised"
        );
        assert_eq!(state.pending_reviews.len(), 1);
        let pr = &state.pending_reviews[0];
        assert_eq!(pr.external_id, "tx_cafe");
        // Payee dimension PRE-SETTLED to the still-valid cached payee.
        assert!(pr.payee_settled(), "payee must be pre-settled");
        assert_eq!(pr.resolve_to, "Local Cafe");
        // Category dimension UNSETTLED -> it will prompt for the category.
        assert!(
            !pr.category_resolved,
            "category must be unsettled (re-prompted), not collapsed to uncategorised"
        );
        assert_eq!(pr.category_suggestion.as_deref(), Some("Utilities"));
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

    // ---- Task 8: end-to-end category integration tests ----

    #[tokio::test]
    async fn high_confidence_imports_with_category() {
        let shim = tempfile::tempdir().unwrap();
        let statedir = tempfile::tempdir().unwrap();
        let state_path = statedir.path().join("s.toml");
        let imports_log = shim.path().join("imports.log");
        let cfg = cfg_at(&state_path);
        let actual = fake_actual(&shim); // payees: Local Cafe; cats: Groceries, Dining
        let notifier = NullNotifier;
        let channel = FakeChannel::new(vec![], 0);

        let mut state = State::default();
        state.monzo.accounts.push(crate::state::AccountLink {
            monzo_id: "acc_1".into(),
            actual_id: "acc".into(),
            ..Default::default()
        });
        let mut bank = FakeBank {
            txs: vec![tx("tx_cafe", "SQ *CAFE", -450)],
        };
        let claude = FakeClaude {
            out: vec![Classification {
                raw: "SQ *CAFE".into(),
                payee: Some("Local Cafe".into()),
                payee_confidence: Confidence::High,
                category: Some("Dining".into()),
                category_confidence: Confidence::High,
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

        let logged = std::fs::read_to_string(&imports_log).unwrap();
        assert!(logged.contains("p_cafe"), "imported under Local Cafe id");
        assert!(
            logged.contains("c_din"),
            "imported with the Dining category id"
        );
        assert!(state.pending_reviews.is_empty(), "nothing held");
        assert_eq!(
            state.category_cache.get("SQ *CAFE").map(String::as_str),
            Some("Dining")
        );
    }

    #[tokio::test]
    async fn uncertain_category_holds_then_imports_after_category_confirm() {
        let shim = tempfile::tempdir().unwrap();
        let statedir = tempfile::tempdir().unwrap();
        let state_path = statedir.path().join("s.toml");
        let imports_log = shim.path().join("imports.log");
        let cfg = cfg_at(&state_path);
        let actual = fake_actual(&shim);
        let notifier = NullNotifier;
        let channel = FakeChannel::new(vec![], 0);

        let mut state = State::default();
        state.monzo.accounts.push(crate::state::AccountLink {
            monzo_id: "acc_1".into(),
            actual_id: "acc".into(),
            ..Default::default()
        });
        let mut bank = FakeBank {
            txs: vec![tx("tx_cafe", "SQ *CAFE", -450)],
        };
        let claude = FakeClaude {
            out: vec![Classification {
                raw: "SQ *CAFE".into(),
                payee: Some("Local Cafe".into()),
                payee_confidence: Confidence::High, // payee confident + exists
                category: Some("Dining".into()),
                category_confidence: Confidence::Low, // category uncertain -> held
            }],
        };
        // Run #1: held (payee pre-settled), category prompt sent, nothing imported.
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
        assert!(
            !imports_log.exists(),
            "must not import while category unsettled"
        );
        assert_eq!(state.pending_reviews.len(), 1);
        assert!(state.pending_reviews[0].category_suggestion.as_deref() == Some("Dining"));
        // Bind the check inside the block so the MutexGuard drops at its last
        // use (before the assert and any later await), satisfying clippy's
        // significant_drop_tightening.
        let category_prompt_sent = {
            let sent = channel.sent.lock().unwrap();
            sent[0].chat_text.to_lowercase().contains("category")
        };
        assert!(category_prompt_sent, "category prompt sent");

        // User confirms the category (what poll would do).
        assert!(state.confirm_category("tx_cafe"));

        // Run #2: no new txs -> the held tx imports with the Dining category.
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
        assert!(logged.contains("tx_cafe") && logged.contains("c_din"));
        assert!(state.pending_reviews.is_empty());
    }

    // ---- Task F6: direct Flex purchase regression ----

    /// A direct Flex purchase (scheme=mastercard on the Flex feed, transfer:None)
    /// must take the ordinary Phase-1/2 path: routed to the Flex Actual account,
    /// categorised normally, NOT treated as a transfer, nothing held.
    #[tokio::test]
    async fn direct_flex_purchase_imports_as_ordinary_spending_on_flex_account() {
        let shim = tempfile::tempdir().unwrap();
        let statedir = tempfile::tempdir().unwrap();
        let state_path = statedir.path().join("s.toml");
        let routed_log = shim.path().join("routed.log");
        let cfg = cfg_at(&state_path);
        let actual = fake_actual_transfers(&shim);
        let notifier = NullNotifier;

        let mut state = State::default();
        state.monzo.accounts.push(AccountLink {
            monzo_id: "acc_flex".into(),
            actual_id: "act-flex".into(),
            is_flex: true,
            ..Default::default()
        });

        // Direct Flex purchase: mastercard on the Flex feed => transfer: None (ordinary).
        let tx = RawTransaction {
            external_id: "tx_dp".into(),
            date: NaiveDate::from_ymd_opt(2026, 6, 1).unwrap(),
            amount_minor: -14997,
            raw_merchant: "Local Cafe".into(),
            source_monzo_id: "acc_flex".into(),
            transfer: None,
        };
        let mut bank = FakeBank { txs: vec![tx] };
        // Confident payee+category so it auto-imports (matches p_cafe / c_groc from the shim).
        let claude = FakeClaude {
            out: vec![Classification {
                raw: "Local Cafe".into(),
                payee: Some("Local Cafe".into()),
                payee_confidence: Confidence::High,
                category: Some("Groceries".into()),
                category_confidence: Confidence::High,
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
            None,
            Confidence::High,
        )
        .await
        .unwrap();

        let logged = std::fs::read_to_string(&routed_log).expect("routed.log must exist");
        let lines: Vec<&str> = logged.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 1, "one ordinary import");
        assert!(
            lines[0].contains("act-flex"),
            "routed to the Flex Actual account"
        );
        assert!(
            lines[0].contains("p_cafe"),
            "ordinary payee, NOT a transfer payee"
        );
        assert!(
            !lines[0].contains("p_xfer"),
            "must not use any transfer payee"
        );
        assert!(
            lines[0].contains("c_groc"),
            "categorised as Groceries (ordinary path includes category)"
        );
        assert!(
            state.held_transfers.is_empty(),
            "not a transfer, nothing held"
        );
    }

    #[tokio::test]
    async fn failed_remap_edits_group_message_once() {
        let _dir = tempfile::tempdir().unwrap();
        let mut state = State::default();
        // Two members of one merchant, both free-text-remapped to a non-existent payee.
        for ext in ["tx1", "tx2"] {
            let mut p = held(ext, "SQ *NEWCAFE");
            p.suggestion = None;
            p.resolve_to = "Nonexistent Payee".into();
            p.telegram_message_id = Some(500); // shared group message
            state.pending_reviews.push(p);
        }
        let channel = FakeChannel::new(vec![], 0);
        let payees = vec!["Tesco".to_string()];
        rehold_failed_remaps(&mut state, Some(&channel as &dyn ConfirmChannel), &payees).await;

        assert_eq!(
            channel.edits(),
            1,
            "edit the shared group message exactly once"
        );
        assert!(
            state
                .pending_reviews
                .iter()
                .all(|p| p.telegram_message_id.is_none())
        );
        assert!(
            state
                .pending_reviews
                .iter()
                .all(|p| p.suggestion.as_deref() == Some("Nonexistent Payee"))
        );
    }
}
