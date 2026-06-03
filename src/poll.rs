//! `aba poll`: drain Telegram replies into the state file. Never touches Actual.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use tracing::info;

use crate::config::Config;
use crate::state::State;
use crate::telegram::{ConfirmChannel, ReplyAction};

/// Apply scripted/real replies to `state`, advancing the Telegram offset.
/// Returns the number of replies applied.
pub async fn poll(
    cfg: &Config,
    state: &mut State,
    channel: &dyn ConfirmChannel,
) -> anyhow::Result<usize> {
    let msg_map: BTreeMap<i64, String> = state
        .pending_reviews
        .iter()
        .filter_map(|p| p.telegram_message_id.map(|m| (m, p.external_id.clone())))
        .collect();
    let (replies, new_offset) = channel
        .poll_replies(state.telegram.last_update_id, &msg_map)
        .await?;

    let valid: BTreeSet<String> = state
        .pending_reviews
        .iter()
        .map(|p| p.external_id.clone())
        .collect();

    let mut applied = 0usize;
    for r in &replies {
        if !valid.contains(&r.entry_key) {
            continue; // stale / spoofed entry key
        }
        let ok = match &r.action {
            ReplyAction::Confirm => state.confirm_pending(&r.entry_key),
            ReplyAction::CreateNew => state.create_pending(&r.entry_key),
            ReplyAction::Skip => state.skip_pending(&r.entry_key),
            ReplyAction::ConfirmCategory => state.confirm_category(&r.entry_key),
            ReplyAction::SkipCategory => state.skip_category(&r.entry_key),
            ReplyAction::Remap(text) => {
                // A free-text remap targets the currently-prompted dimension:
                // payee if the payee dimension is still unsettled, else category.
                let payee_settled = state
                    .pending_reviews
                    .iter()
                    .find(|p| p.external_id == r.entry_key)
                    .is_some_and(crate::state::PendingReview::payee_settled);
                if payee_settled {
                    state.remap_category(&r.entry_key, text)
                } else {
                    state.remap_pending(&r.entry_key, text)
                }
            }
        };
        if ok {
            // A payee-settling action can transition the payee dimension from
            // unsettled to settled while the category dimension is still open.
            // When that happens, clear the prompt id so run() re-prompts (now for
            // the category). Scope this to payee actions: category actions can
            // never trigger this transition, and Skip removes the entry entirely.
            if matches!(
                &r.action,
                ReplyAction::Confirm | ReplyAction::CreateNew | ReplyAction::Remap(_)
            ) && let Some(pr) = state
                .pending_reviews
                .iter_mut()
                .find(|p| p.external_id == r.entry_key)
                && pr.payee_settled()
                && !pr.category_resolved
            {
                pr.telegram_message_id = None;
            }
            applied += 1;
        }
    }

    state.telegram.last_update_id = new_offset;
    state.save(&cfg.state_path)?;
    info!("poll applied {applied} repl(ies); offset now {new_offset}");
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::tests::FakeChannel;
    use crate::telegram::Reply;
    use tempfile::tempdir;

    fn cfg_with_state(path: &std::path::Path) -> Config {
        Config::from_getter(|k| match k {
            "ANTHROPIC_API_KEY" => Some("ak".into()),
            "MONZO_CLIENT_ID" => Some("c".into()),
            "MONZO_CLIENT_SECRET" => Some("s".into()),
            "MONZO_REDIRECT_URI" => Some("http://localhost/cb".into()),
            "APPRISE_URL" => Some("http://a/notify".into()),
            "ACTUAL_SERVER_URL" => Some("http://actual".into()),
            "ACTUAL_PASSWORD" => Some("pw".into()),
            "ACTUAL_SYNC_ID" => Some("sync".into()),
            "ABA_STATE_PATH" => Some(path.to_string_lossy().to_string()),
            _ => None,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn category_confirm_settles_category_dimension() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.toml");
        let cfg = cfg_with_state(&path);

        let mut state = State::default();
        state.pending_reviews.push(crate::state::PendingReview {
            external_id: "tx1".into(),
            source_monzo_id: "acc_1".into(),
            raw_merchant: "SQ *CAFE".into(),
            amount: -450,
            date: "2026-06-01".into(),
            resolve_to: "Local Cafe".into(), // payee settled
            create_new: false,
            suggestion: None,
            confidence: None,
            category_suggestion: Some("Dining".into()),
            category_confidence: Some("medium".into()),
            resolve_category_to: String::new(),
            category_resolved: false,
            telegram_message_id: Some(900),
        });

        let channel = FakeChannel::new(
            vec![Reply {
                entry_key: "tx1".into(),
                action: ReplyAction::ConfirmCategory,
            }],
            55,
        );
        let applied = poll(&cfg, &mut state, &channel).await.unwrap();
        assert_eq!(applied, 1);
        assert!(state.pending_reviews[0].category_resolved);
        assert_eq!(state.pending_reviews[0].resolve_category_to, "Dining");
    }

    #[tokio::test]
    async fn freetext_remap_targets_category_when_payee_settled() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.toml");
        let cfg = cfg_with_state(&path);
        let mut state = State::default();
        state.pending_reviews.push(crate::state::PendingReview {
            external_id: "tx2".into(),
            source_monzo_id: "acc_1".into(),
            raw_merchant: "X".into(),
            amount: -10,
            date: "2026-06-01".into(),
            resolve_to: "X Payee".into(), // payee settled
            create_new: false,
            suggestion: None,
            confidence: None,
            category_suggestion: Some("Dining".into()),
            category_confidence: Some("low".into()),
            resolve_category_to: String::new(),
            category_resolved: false,
            telegram_message_id: Some(901),
        });
        let channel = FakeChannel::new(
            vec![Reply {
                entry_key: "tx2".into(),
                action: ReplyAction::Remap("Groceries".into()),
            }],
            60,
        );
        let applied = poll(&cfg, &mut state, &channel).await.unwrap();
        assert_eq!(applied, 1);
        assert!(state.pending_reviews[0].category_resolved);
        assert_eq!(state.pending_reviews[0].resolve_category_to, "Groceries");
    }

    #[tokio::test]
    async fn applies_confirm_and_ignores_unknown_key() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.toml");
        let cfg = cfg_with_state(&path);

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

        let channel = FakeChannel::new(
            vec![
                Reply {
                    entry_key: "tx1".into(),
                    action: ReplyAction::Confirm,
                },
                Reply {
                    entry_key: "ghost".into(),
                    action: ReplyAction::Skip,
                },
            ],
            123,
        );

        let applied = poll(&cfg, &mut state, &channel).await.unwrap();
        assert_eq!(applied, 1);
        assert_eq!(state.pending_reviews[0].resolve_to, "Local Cafe");
        assert_eq!(state.telegram.last_update_id, 123);

        // persisted
        let reloaded = State::load(&path).unwrap();
        assert_eq!(reloaded.telegram.last_update_id, 123);
        assert_eq!(reloaded.pending_reviews[0].resolve_to, "Local Cafe");
    }
}
