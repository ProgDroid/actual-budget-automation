use std::collections::BTreeMap;

use anyhow::Result;
use chrono::NaiveDate;
use tracing::warn;

use crate::actual::ActualClient;
use crate::actual::shim_protocol::{Category, ImportTxn, Payee, ScheduleRow};
use crate::banks::RawTransaction;
use crate::matching::Schedule;
use crate::state::{AccountLink, ClassifiedHold, State};

use super::classify::{HeldClassification, category_id_for, payee_id_for};
use super::transfer::actual_id_for;

/// Hold a transaction whose source Monzo account is not mapped to an Actual
/// account (no routing target yet). Payee/category default to unsettled.
fn hold_unmapped(state: &mut State, tx: &RawTransaction, held: &mut usize) {
    let date = tx.date.format("%Y-%m-%d").to_string();
    if state.add_pending(
        &tx.external_id,
        &tx.raw_merchant,
        tx.amount_minor,
        &date,
        &tx.source_monzo_id,
    ) {
        *held += 1;
    }
}

/// Build `Schedule`s for matching by resolving each row's payee id to a name.
pub fn schedules_from_rows(rows: Vec<ScheduleRow>, payees: &[Payee]) -> Vec<Schedule> {
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

/// Read-only inputs to import-row assembly (borrowed to avoid cloning the
/// candidate set + Actual catalog).
pub struct AssembleInputs<'a> {
    /// All non-scheduled candidates this run (used to resolve `external_id` -> tx).
    pub candidates: &'a [RawTransaction],
    /// FULL cache hits: (`external_id`, `payee_name`, `category_name`). Empty category = uncategorised.
    pub cached: &'a [(String, String, String)],
    /// Claude matches confident in BOTH dimensions: (`external_id`, `payee_name`, `category_name`).
    pub matched: &'a [(String, String, String)],
    /// Held classifications (>=1 uncertain dimension) to persist as pending reviews.
    pub held_classifications: &'a [HeldClassification],
    pub payees: &'a [Payee],
    pub categories: &'a [Category],
    pub account_links: &'a [AccountLink],
    /// Dry-run suppresses payee creation on the resolved path.
    pub dry_run: bool,
}

/// What assembly produced: import rows (keyed by Actual account id), the count of
/// held items, and merchant->payee pairs newly folded into the cache.
pub struct Assembled {
    pub grouped: BTreeMap<String, Vec<ImportTxn>>,
    pub held: usize,
    pub newly_cached: Vec<(String, String)>,
}

/// Assemble import rows from cached hits, fresh Claude matches, previously
/// resolved pending reviews, and newly held classifications. Mutates `state`
/// (payee/category caches, pending reviews) and may call `actual.create_payee`
/// for resolved `create_new` reviews. Fail-loud throughout: anything that no
/// longer resolves is re-held, never silently dropped or mis-imported.
#[allow(clippy::too_many_lines)]
pub async fn assemble(
    inputs: AssembleInputs<'_>,
    state: &mut State,
    actual: &ActualClient,
) -> Result<Assembled> {
    let mut grouped: BTreeMap<String, Vec<ImportTxn>> = BTreeMap::new();
    let mut held = 0usize;
    let mut newly_cached: Vec<(String, String)> = Vec::new();

    // Apply already-resolved pending reviews from previous runs. Taken HERE (not
    // in run()) so the resolved loop + held loop share one ordering. (Nothing
    // between rehold_failed_remaps and here reads pending_reviews, so moving the
    // take past plan/claude/apply_matches is behaviour-neutral.)
    let resolved = state.take_resolved();

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
            category_id_for(inputs.categories, category_name).map(|id| Some(id.to_string()))
        }
    };

    let by_id: BTreeMap<&str, &RawTransaction> = inputs
        .candidates
        .iter()
        .map(|t| (t.external_id.as_str(), t))
        .collect();

    // cached hits (full: payee + category)
    for (ext_id, payee_name, category_name) in inputs.cached {
        let cat_id = resolve_cat(category_name);
        match (
            by_id.get(ext_id.as_str()),
            payee_id_for(inputs.payees, payee_name),
        ) {
            (Some(tx), Some(pid)) => {
                if let Some(cat) = &cat_id {
                    if let Some(aid) = actual_id_for(&tx.source_monzo_id, inputs.account_links) {
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
    for (ext_id, payee_name, category_name) in inputs.matched {
        let cat_id = resolve_cat(category_name);
        match (
            by_id.get(ext_id.as_str()),
            payee_id_for(inputs.payees, payee_name),
        ) {
            (Some(tx), Some(pid)) if cat_id.is_some() => {
                let cat = cat_id.unwrap();
                if let Some(aid) = actual_id_for(&tx.source_monzo_id, inputs.account_links) {
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
        let pid = match payee_id_for(inputs.payees, &r.payee_name) {
            Some(pid) => pid.to_string(),
            None if r.create_new && !inputs.dry_run => {
                actual.create_payee(r.payee_name.clone()).await?.id
            }
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
                if let Some(id) = category_id_for(inputs.categories, name) {
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
        if let Some(aid) = actual_id_for(&r.source_monzo_id, inputs.account_links) {
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
    for h in inputs.held_classifications {
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

    Ok(Assembled {
        grouped,
        held,
        newly_cached,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::actual::shim_protocol::{Payee, ScheduleRow};

    #[test]
    fn schedules_from_rows_drops_bad_date_and_missing_payee_loudly() {
        let payees = vec![Payee {
            id: "p1".into(),
            name: "Rent Co".into(),
            transfer_acct: None,
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
        let schedules = schedules_from_rows(rows, &payees);
        assert_eq!(schedules.len(), 1, "only the fully-valid row survives");
        assert_eq!(schedules[0].payee, "Rent Co");
    }
}
