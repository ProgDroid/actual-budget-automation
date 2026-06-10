use tracing::warn;

use crate::actual::shim_protocol::{ImportTxn, Payee};
use crate::banks::{RawTransaction, TransferTarget};
use crate::state::{AccountLink, HeldTransfer, PotLink, State};

/// Resolve the Actual account id for a source Monzo account.
/// Falls back to the sole mapped account when there's exactly one (covers
/// single-account users and legacy held items with an empty source). Returns
/// `None` when the source is unknown and the mapping is ambiguous → caller holds.
pub fn actual_id_for(source_monzo_id: &str, links: &[AccountLink]) -> Option<String> {
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

/// Find the transfer payee id for an Actual account (a payee whose
/// `transfer_acct` equals `actual_id`). Actual auto-creates one per account.
pub fn transfer_payee_id<'a>(payees: &'a [Payee], actual_id: &str) -> Option<&'a str> {
    payees
        .iter()
        .find(|p| p.transfer_acct.as_deref() == Some(actual_id))
        .map(|p| p.id.as_str())
}

/// Resolve a transfer's destination to an Actual account id. `Pot` looks up
/// `[[monzo.pots]]`; `Account` looks up `[[monzo.accounts]]`. Empty `actual_id`
/// (present but unmapped) and a missing entry both yield `None` (caller holds).
pub fn resolve_transfer_dest(
    target: &TransferTarget,
    pots: &[PotLink],
    accounts: &[AccountLink],
) -> Option<String> {
    match target {
        TransferTarget::Pot(pot_id) => pots
            .iter()
            .find(|p| &p.monzo_pot_id == pot_id && !p.actual_id.is_empty())
            .map(|p| p.actual_id.clone()),
        TransferTarget::Account(acc_id) => accounts
            .iter()
            .find(|a| &a.monzo_id == acc_id && !a.actual_id.is_empty())
            .map(|a| a.actual_id.clone()),
        // F5 suppresses FlexMirror when the main-side twin is present; until
        // then it is held (None = unresolved destination).
        TransferTarget::FlexMirror => None,
    }
}

/// Reconcile this run's Flex mirror lines against main-side Flex transfers.
///
/// A `monzo_flex` line on the Flex feed mirrors a main-side transfer. Splits the
/// `FlexMirror` rows out of `transfer_rows`, suppresses any whose twin (same abs
/// amount + date) is a main-side `Account(flex)` transfer THIS run, and
/// (re)enqueues unpaired mirrors onto `state.held_transfers` — re-evaluating prior
/// held mirrors too, so a mirror held in an earlier run resolves once its twin
/// arrives. Returns the remaining non-mirror transfer rows and the suppressed count.
pub fn reconcile_flex_mirrors(
    state: &mut State,
    transfer_rows: Vec<RawTransaction>,
) -> (Vec<RawTransaction>, usize) {
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

    // Split mirrors out so the Account/Pot intent loop never sees them.
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
    (transfer_rows, flex_mirrors_suppressed)
}

/// Resolve transfer intents (the retry queue in `state.held_transfers` + this
/// run's freshly-fetched transfers, deduped by `external_id`) to Actual import
/// rows. Each resolved transfer becomes ONE uncategorised row in the SOURCE
/// account's group (Actual auto-creates the linked counterpart). Intents whose
/// source and/or destination is still unmapped are written back to
/// `state.held_transfers` for next-run retry. Returns (rows keyed by source
/// Actual account id, count imported).
///
/// NOTE: `flex_mirror` `HeldTransfer`s enqueued by `reconcile_flex_mirrors` are
/// deliberately re-cloned here (via `state.held_transfers`) and re-held —
/// `FlexMirror` -> `resolve_transfer_dest` None -> `still_held`. So
/// `reconcile_flex_mirrors` MUST run before this.
pub fn resolve_intents(
    state: &mut State,
    transfer_rows: &[RawTransaction],
    account_links: &[AccountLink],
    pot_links: &[PotLink],
    payees: &[Payee],
) -> (Vec<(String, ImportTxn)>, usize) {
    let mut rows: Vec<(String, ImportTxn)> = Vec::new();
    let mut transfer_intents: Vec<HeldTransfer> = state.held_transfers.clone();
    for row in transfer_rows {
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
        let dest = resolve_transfer_dest(&target, pot_links, account_links);
        let src = actual_id_for(&ht.source_monzo_id, account_links);
        match (src, dest) {
            (Some(src_actual), Some(dest_actual)) => {
                if let Some(pid) = transfer_payee_id(payees, &dest_actual) {
                    rows.push((
                        src_actual,
                        ImportTxn {
                            date: ht.date.clone(),
                            amount: ht.amount,
                            payee: pid.to_string(),
                            category: None, // transfers are uncategorised
                            imported_id: ht.external_id.clone(),
                        },
                    ));
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
    (rows, transfers_imported)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn transfer_payee_id_finds_the_account_transfer_payee() {
        use crate::actual::shim_protocol::Payee;
        let payees = vec![
            Payee {
                id: "p1".into(),
                name: "Tesco".into(),
                transfer_acct: None,
            },
            Payee {
                id: "p2".into(),
                name: "Transfer: Savings".into(),
                transfer_acct: Some("act-pot".into()),
            },
        ];
        assert_eq!(transfer_payee_id(&payees, "act-pot"), Some("p2"));
        assert_eq!(transfer_payee_id(&payees, "act-missing"), None);
    }

    #[test]
    fn resolve_transfer_dest_maps_pot_then_account() {
        use crate::banks::TransferTarget;
        let pots = vec![PotLink {
            monzo_pot_id: "pot_abc".into(),
            monzo_pot_name: "Savings".into(),
            actual_id: "act-pot".into(),
            actual_name: "Savings".into(),
        }];
        let accts = vec![AccountLink {
            monzo_id: "acc_joint".into(),
            monzo_name: "Joint".into(),
            actual_id: "act-joint".into(),
            actual_name: "Joint".into(),
            is_flex: false,
        }];
        assert_eq!(
            resolve_transfer_dest(&TransferTarget::Pot("pot_abc".into()), &pots, &accts),
            Some("act-pot".to_string())
        );
        assert_eq!(
            resolve_transfer_dest(&TransferTarget::Account("acc_joint".into()), &pots, &accts),
            Some("act-joint".to_string())
        );
        // FlexMirror has no resolvable destination (compile-forced default) -> None.
        // Guards against accidental change in F5.
        assert_eq!(
            resolve_transfer_dest(&TransferTarget::FlexMirror, &pots, &accts),
            None
        );
        // unmapped pot -> None
        assert_eq!(
            resolve_transfer_dest(&TransferTarget::Pot("pot_unmapped".into()), &pots, &accts),
            None
        );
        // a pot present but with empty actual_id is treated as unmapped -> None
        let pots_empty = vec![PotLink {
            monzo_pot_id: "pot_x".into(),
            monzo_pot_name: "X".into(),
            actual_id: String::new(),
            actual_name: String::new(),
        }];
        assert_eq!(
            resolve_transfer_dest(&TransferTarget::Pot("pot_x".into()), &pots_empty, &accts),
            None
        );
    }
}
