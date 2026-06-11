use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::actual::shim_protocol::ImportTxn;
use crate::ids::ActualAccountId;
use crate::pipeline::HeldClassification;

/// Counts the dry-run report prints alongside the per-account row breakdown.
pub struct DryRunCounts {
    pub would_skip: usize,
    pub held: usize,
    pub transfers_imported: usize,
    pub transfers_held: usize,
    pub flex_mirrors_suppressed: usize,
}

/// Print the dry-run report. Pure output; no writes. The grouped import rows,
/// held classifications, and newly-cached pairs are exactly what a real run
/// WOULD persist.
pub fn dry_run(
    grouped: &BTreeMap<ActualAccountId, Vec<ImportTxn>>,
    held_classifications: &[HeldClassification],
    newly_cached: &[(String, String)],
    counts: &DryRunCounts,
) {
    let total: usize = grouped.values().map(Vec::len).sum();
    println!("DRY RUN — no writes to Actual");
    println!("  would import : {total}");
    println!(
        "  would skip   : {} (matched a schedule)",
        counts.would_skip
    );
    println!("  would hold   : {}", counts.held);
    println!("  would xfer-import : {}", counts.transfers_imported);
    println!("  would xfer-hold   : {}", counts.transfers_held);
    println!(
        "  flex mirrors suppressed: {}",
        counts.flex_mirrors_suppressed
    );
    for (aid, rows) in grouped {
        println!("  -> account {aid}: {} txns", rows.len());
        for r in rows {
            println!("    + {} {} -> payee {}", r.date, r.amount, r.payee);
        }
    }
    for h in held_classifications {
        let p = h
            .payee_resolved
            .as_deref()
            .or(h.payee_suggestion.as_deref())
            .unwrap_or("?");
        let c = h
            .category_resolved
            .as_deref()
            .or(h.category_suggestion.as_deref())
            .unwrap_or("?");
        println!(
            "    ? hold {} {} (payee {p}, category {c})",
            h.tx.raw_merchant, h.tx.amount_minor
        );
    }
    if !newly_cached.is_empty() {
        println!("  would cache  : {}", newly_cached.len());
        for (raw, payee) in newly_cached {
            println!("    ~ {raw} -> {payee}");
        }
    }
}

/// Build the Apprise notification body for a completed (non-dry) run.
#[must_use]
pub fn summary_body(
    added: usize,
    updated: usize,
    held: usize,
    newly_cached: &[(String, String)],
    awaiting: usize,
    transfers_imported: usize,
    held_transfers: usize,
) -> String {
    let mut body = format!("Added {added}, updated {updated}, held {held} for review.");
    if !newly_cached.is_empty() {
        let _ = write!(body, "\nNewly auto-matched ({}):", newly_cached.len());
        for (raw, payee) in newly_cached {
            let _ = write!(body, "\n  {raw} -> {payee}");
        }
    }
    if awaiting > 0 {
        let _ = write!(body, "\n{awaiting} merchant(s) awaiting your confirmation.");
    }
    if transfers_imported > 0 {
        let _ = write!(body, "\nImported {transfers_imported} transfer(s).");
    }
    if held_transfers > 0 {
        let _ = write!(
            body,
            "\n\u{2922} {held_transfers} transfer(s) awaiting account mapping \u{2014} run `aba accounts`.",
        );
    }
    body
}
