use anyhow::Context as _;
use tracing::warn;

use crate::actual::ActualClient;
use crate::banks::{BankError, BankSource as _};
use crate::config::Config;
use crate::state::{AccountLink, PotLink, State};

use super::persist_rotated_token;

/// Minimum normalised-name similarity for `aba accounts` to auto-link a Monzo
/// account to an Actual account; below this the row is left unmapped.
const ACCOUNT_MATCH_THRESHOLD: f64 = 0.6;

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
    monzo: &[(String, String)],
    actual: &[(String, String)],
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
    pots: &[(String, String)],
    actual: &[(String, String)],
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
                    actual_id: aid.clone(),
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
    let actual_accounts: Vec<(String, String)> = actual
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
        if l.actual_id.is_empty() {
            println!(
                "  {} ({})  →  [UNMAPPED — edit state file]{}",
                l.monzo_name, l.monzo_id, flex
            );
        } else {
            println!(
                "  {} ({})  →  {} ({}){}",
                l.monzo_name, l.monzo_id, l.actual_name, l.actual_id, flex
            );
        }
    }

    // Collect pots across all current accounts, then suggest Pot → Actual links.
    let mut all_pots: Vec<(String, String)> = Vec::new();
    for (mid, _) in &monzo_accounts {
        match bank.list_pots(mid).await {
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
            if l.actual_id.is_empty() {
                println!(
                    "  {} ({})  →  [UNMAPPED — edit state file]",
                    l.monzo_pot_name, l.monzo_pot_id
                );
            } else {
                println!(
                    "  {} ({})  →  {} ({})",
                    l.monzo_pot_name, l.monzo_pot_id, l.actual_name, l.actual_id
                );
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
    fn map_accounts_flags_flex_account_by_description_prefix() {
        let monzo = vec![
            ("acc_cur".to_string(), "user_0000".to_string()),
            ("acc_flex".to_string(), "monzoflex_0000".to_string()),
        ];
        let actual = vec![
            ("u-cur".to_string(), "Current".to_string()),
            ("u-flex".to_string(), "Flex".to_string()),
        ];
        let links = suggest_links_with_flex(&monzo, &actual, ACCOUNT_MATCH_THRESHOLD);
        let flex = links.iter().find(|l| l.monzo_id == "acc_flex").unwrap();
        assert!(flex.is_flex);
        let cur = links.iter().find(|l| l.monzo_id == "acc_cur").unwrap();
        assert!(!cur.is_flex);
    }

    #[test]
    fn suggest_pot_links_matches_by_name_and_leaves_unmatched_unmapped() {
        let pots = vec![
            ("pot_abc".to_string(), "Savings".to_string()),
            ("pot_xyz".to_string(), "Holiday".to_string()),
        ];
        let actual = vec![("act-sav".to_string(), "Savings".to_string())];
        let links = suggest_pot_links(&pots, &actual, ACCOUNT_MATCH_THRESHOLD);
        assert_eq!(links.len(), 2);
        let sav = links.iter().find(|l| l.monzo_pot_id == "pot_abc").unwrap();
        assert_eq!(sav.actual_id, "act-sav");
        assert_eq!(sav.actual_name, "Savings");
        let hol = links.iter().find(|l| l.monzo_pot_id == "pot_xyz").unwrap();
        assert!(hol.actual_id.is_empty()); // no match -> unmapped
        assert_eq!(hol.monzo_pot_name, "Holiday");
    }
}
