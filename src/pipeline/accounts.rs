use anyhow::Context as _;
use tracing::warn;

use crate::actual::ActualClient;
use crate::banks::{BankError, BankSource as _};
use crate::config::Config;
use crate::ids::{ActualAccountId, MonzoAccountId, MonzoPotId};
use crate::state::{AccountLink, PotLink, State};

use super::auth::persist_rotated_token;

/// Minimum normalised-name similarity for `aba accounts` to auto-link a Monzo
/// account to an Actual account; below this the row is left unmapped.
const ACCOUNT_MATCH_THRESHOLD: f64 = 0.6;

/// Auto-suggest Monzo→Actual links by normalised-name similarity. Each Monzo
/// account is linked to its best Actual match above `threshold`; below it the
/// link is left unmapped (`actual_id` empty) for the user to fill in.
fn suggest_links(
    monzo: &[(MonzoAccountId, String)],
    actual: &[(ActualAccountId, String)],
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
                    actual_id: Some(aid.clone()),
                    actual_name: aname.clone(),
                    is_flex: false,
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

/// `suggest_links` plus Flex auto-detection: flag any link whose Monzo
/// description (stored in `monzo_name`) starts with `monzoflex_`.
fn suggest_links_with_flex(
    monzo: &[(MonzoAccountId, String)],
    actual: &[(ActualAccountId, String)],
    threshold: f64,
) -> Vec<AccountLink> {
    let mut links = suggest_links(monzo, actual, threshold);
    // Defensive: list_accounts already excludes monzoflexbackingloan_ (is_mappable_account
    // is the authoritative gate), but guard here too so this stays correct if called with
    // a raw account list.
    for l in &mut links {
        if l.monzo_name.starts_with("monzoflex_")
            && !l.monzo_name.starts_with("monzoflexbackingloan_")
        {
            l.is_flex = true;
        }
    }
    links
}

/// Auto-suggest Monzo-pot → Actual-account links by normalised-name similarity.
/// Same policy as `suggest_links`: best match above `threshold`, else unmapped.
fn suggest_pot_links(
    pots: &[(MonzoPotId, String)],
    actual: &[(ActualAccountId, String)],
    threshold: f64,
) -> Vec<PotLink> {
    pots.iter()
        .map(|(pid, pname)| {
            let best = actual
                .iter()
                .map(|(aid, aname)| (aid, aname, crate::matching::payee_similarity(pname, aname)))
                .filter(|(_, _, score)| *score >= threshold)
                .max_by(|a, b| a.2.total_cmp(&b.2));
            match best {
                Some((aid, aname, _)) => PotLink {
                    monzo_pot_id: pid.clone(),
                    monzo_pot_name: pname.clone(),
                    actual_id: Some(aid.clone()),
                    actual_name: aname.clone(),
                },
                None => PotLink {
                    monzo_pot_id: pid.clone(),
                    monzo_pot_name: pname.clone(),
                    ..PotLink::default()
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
    let actual_accounts: Vec<(ActualAccountId, String)> = actual
        .get_accounts()
        .await
        .context("get_accounts")?
        .into_iter()
        .map(|a| (a.id, a.name))
        .collect();

    let links = suggest_links_with_flex(&monzo_accounts, &actual_accounts, ACCOUNT_MATCH_THRESHOLD);

    println!("Suggested Monzo → Actual account mapping:");
    for l in &links {
        let flex = if l.is_flex { "  [FLEX]" } else { "" };
        match &l.actual_id {
            Some(aid) => println!(
                "  {} ({})  →  {} ({}){}",
                l.monzo_name, l.monzo_id, l.actual_name, aid, flex
            ),
            None => println!(
                "  {} ({})  →  [UNMAPPED — edit state file]{}",
                l.monzo_name, l.monzo_id, flex
            ),
        }
    }

    // Collect pots across all current accounts, then suggest Pot → Actual links.
    let mut all_pots: Vec<(MonzoPotId, String)> = Vec::new();
    for (mid, _) in &monzo_accounts {
        match bank.list_pots(mid.as_str()).await {
            Ok(pots) => all_pots.extend(pots),
            Err(e) => warn!("could not list pots for {mid}: {}", bank_err_text(&e)),
        }
    }
    let pot_links = suggest_pot_links(&all_pots, &actual_accounts, ACCOUNT_MATCH_THRESHOLD);
    if pot_links.is_empty() {
        println!("\nNo Monzo pots found to map.");
    } else {
        println!("\nSuggested Monzo pot → Actual account mapping:");
        for l in &pot_links {
            match &l.actual_id {
                Some(aid) => println!(
                    "  {} ({})  →  {} ({})",
                    l.monzo_pot_name, l.monzo_pot_id, l.actual_name, aid
                ),
                None => println!(
                    "  {} ({})  →  [UNMAPPED — edit state file]",
                    l.monzo_pot_name, l.monzo_pot_id
                ),
            }
        }
    }

    if write {
        state.monzo.accounts = links;
        state.monzo.pots = pot_links;
        state.save(&cfg.state_path)?;
        println!(
            "\nWrote {} account link(s) and {} pot link(s) to {}.",
            state.monzo.accounts.len(),
            state.monzo.pots.len(),
            cfg.state_path.display()
        );
        println!(
            "Review/override UNMAPPED or wrong rows by editing [[monzo.accounts]] / [[monzo.pots]] there."
        );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggest_links_matches_by_name_and_leaves_unmatched_unmapped() {
        let monzo = vec![
            (MonzoAccountId::from("acc_1"), "Personal".to_string()),
            (MonzoAccountId::from("acc_2"), "Joint Account".to_string()),
            (MonzoAccountId::from("acc_x"), "Nothing Like It".to_string()),
        ];
        let actual = vec![
            (ActualAccountId::from("u1"), "Monzo Personal".to_string()),
            (ActualAccountId::from("u2"), "Monzo Joint".to_string()),
        ];
        let links = suggest_links(&monzo, &actual, 0.6);
        let p = links
            .iter()
            .find(|l| l.monzo_id.as_str() == "acc_1")
            .unwrap();
        assert_eq!(p.actual_id, Some(ActualAccountId::from("u1")));
        let j = links
            .iter()
            .find(|l| l.monzo_id.as_str() == "acc_2")
            .unwrap();
        assert_eq!(j.actual_id, Some(ActualAccountId::from("u2")));
        let x = links
            .iter()
            .find(|l| l.monzo_id.as_str() == "acc_x")
            .unwrap();
        assert_eq!(x.actual_id, None, "no confident match → unmapped");
    }

    #[test]
    fn map_accounts_flags_flex_account_by_description_prefix() {
        let monzo = vec![
            (MonzoAccountId::from("acc_cur"), "user_0000".to_string()),
            (
                MonzoAccountId::from("acc_flex"),
                "monzoflex_0000".to_string(),
            ),
        ];
        let actual = vec![
            (ActualAccountId::from("u-cur"), "Current".to_string()),
            (ActualAccountId::from("u-flex"), "Flex".to_string()),
        ];
        let links = suggest_links_with_flex(&monzo, &actual, ACCOUNT_MATCH_THRESHOLD);
        let flex = links
            .iter()
            .find(|l| l.monzo_id.as_str() == "acc_flex")
            .unwrap();
        assert!(flex.is_flex);
        let cur = links
            .iter()
            .find(|l| l.monzo_id.as_str() == "acc_cur")
            .unwrap();
        assert!(!cur.is_flex);
    }

    #[test]
    fn suggest_pot_links_matches_by_name_and_leaves_unmatched_unmapped() {
        let pots = vec![
            (MonzoPotId::from("pot_abc"), "Savings".to_string()),
            (MonzoPotId::from("pot_xyz"), "Holiday".to_string()),
        ];
        let actual = vec![(ActualAccountId::from("act-sav"), "Savings".to_string())];
        let links = suggest_pot_links(&pots, &actual, ACCOUNT_MATCH_THRESHOLD);
        assert_eq!(links.len(), 2);
        let sav = links
            .iter()
            .find(|l| l.monzo_pot_id.as_str() == "pot_abc")
            .unwrap();
        assert_eq!(sav.actual_id, Some(ActualAccountId::from("act-sav")));
        assert_eq!(sav.actual_name, "Savings");
        let hol = links
            .iter()
            .find(|l| l.monzo_pot_id.as_str() == "pot_xyz")
            .unwrap();
        assert_eq!(hol.actual_id, None); // no match -> unmapped
        assert_eq!(hol.monzo_pot_name, "Holiday");
    }
}
