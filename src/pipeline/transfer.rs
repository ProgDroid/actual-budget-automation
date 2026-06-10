use crate::actual::shim_protocol::Payee;
use crate::banks::TransferTarget;
use crate::state::{AccountLink, PotLink};

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
