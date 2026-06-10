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
    use crate::telegram::group_key;

    // A prompt's message_id maps to the merchant group it was sent for, so a
    // free-text reply correlates to the whole group.
    let msg_map: BTreeMap<i64, String> = state
        .pending_reviews
        .iter()
        .filter_map(|p| {
            p.telegram_message_id
                .map(|m| (m, group_key(&p.raw_merchant)))
        })
        .collect();
    let (replies, new_offset) = channel
        .poll_replies(state.telegram.last_update_id, &msg_map)
        .await?;

    // Only groups we actually prompted are resolvable; msg_map's values are
    // exactly those group keys. (A new member that joined an outstanding group
    // shares its key, so fan-out still reaches it.)
    let valid: BTreeSet<String> = msg_map.values().cloned().collect();

    let mut applied = 0usize;
    for r in &replies {
        if !valid.contains(&r.entry_key) {
            continue; // stale / spoofed group key
        }
        // All member external_ids for this merchant group.
        let members: Vec<String> = state
            .pending_reviews
            .iter()
            .filter(|p| group_key(&p.raw_merchant) == r.entry_key)
            .map(|p| p.external_id.clone())
            .collect();
        // Members share raw_merchant, hence the same dimension/suggestion; the
        // first member decides whether a free-text remap targets payee or category.
        // Hoisted out of the action match: Remap needs it, and computing it here avoids
        // a second immutable borrow inside the loop where `state` is mutably borrowed.
        let payee_settled = state
            .pending_reviews
            .iter()
            .find(|p| group_key(&p.raw_merchant) == r.entry_key)
            .is_some_and(crate::state::PendingReview::payee_settled);

        let mut any = false;
        // `members` is collected before any mutation, so skip_pending's retain() is safe.
        for ext in &members {
            let ok = match &r.action {
                ReplyAction::Confirm => state.confirm_pending(ext),
                ReplyAction::CreateNew => state.create_pending(ext),
                ReplyAction::Skip => state.skip_pending(ext),
                ReplyAction::ConfirmCategory => state.confirm_category(ext),
                ReplyAction::SkipCategory => state.skip_category(ext),
                ReplyAction::Remap(text) => {
                    if payee_settled {
                        state.remap_category(ext, text)
                    } else {
                        state.remap_pending(ext, text)
                    }
                }
            };
            any |= ok;
        }
        if any {
            // A payee-settling action with the category still open clears the
            // group's prompt id (all members) so run() re-prompts the whole group
            // for the category dimension next run.
            if matches!(
                &r.action,
                ReplyAction::Confirm | ReplyAction::CreateNew | ReplyAction::Remap(_)
            ) {
                for p in state
                    .pending_reviews
                    .iter_mut()
                    .filter(|p| group_key(&p.raw_merchant) == r.entry_key)
                {
                    if p.payee_settled() && !p.category_resolved {
                        p.telegram_message_id = None;
                    }
                }
            }
            applied += 1;
        }
    }

    state.telegram.last_update_id = new_offset;
    state.save(&cfg.state_path)?;
    info!("poll applied {applied} group(s); offset now {new_offset}");
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::tests::FakeChannel;
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
                entry_key: crate::telegram::group_key("SQ *CAFE"),
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
                entry_key: crate::telegram::group_key("X"),
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
                    entry_key: crate::telegram::group_key("SQ *CAFE"),
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

    #[tokio::test]
    async fn heterogeneous_group_payee_confirm_is_safe() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.toml");
        let cfg = cfg_with_state(&path);
        let mut state = State::default();

        // Member A: payee unsettled (Claude suggestion pending).
        // add_pending_full 7th arg is source_monzo_id, not resolve_to;
        // resolve_to is always initialised to "". We pass "" for monzo_id here.
        state.add_pending_full(
            "txA",
            "SQ *CAFE",
            -100,
            "2026-06-01",
            Some("Local Cafe".into()),
            Some("medium".into()),
            "",
        );

        // Member B: same merchant but payee already settled, category open.
        // Must be a struct literal because add_pending_full cannot set resolve_to.
        state.pending_reviews.push(crate::state::PendingReview {
            external_id: "txB".into(),
            source_monzo_id: String::new(),
            raw_merchant: "SQ *CAFE".into(),
            amount: -200,
            date: "2026-06-02".into(),
            resolve_to: "Local Cafe".into(),
            create_new: false,
            suggestion: None,
            confidence: None,
            category_suggestion: None,
            category_confidence: None,
            resolve_category_to: String::new(),
            category_resolved: false,
            telegram_message_id: None,
        });

        for p in &mut state.pending_reviews {
            p.telegram_message_id = Some(910);
        }

        let gk = crate::telegram::group_key("SQ *CAFE");
        let channel = crate::pipeline::tests::FakeChannel::new(
            vec![Reply {
                entry_key: gk,
                action: ReplyAction::Confirm,
            }],
            88,
        );
        let applied = poll(&cfg, &mut state, &channel).await.unwrap();
        assert_eq!(applied, 1, "one group applied");
        assert_eq!(state.pending_reviews[0].resolve_to, "Local Cafe");
        // Member B was already payee-settled; Confirm must leave its payee untouched.
        assert_eq!(state.pending_reviews[1].resolve_to, "Local Cafe");
        // Member B has an open category, so the payee-settling Confirm must clear its
        // prompt id — that is the heterogeneous-group payoff: run() re-prompts the
        // group for the category dimension next run.
        assert!(
            state.pending_reviews[1].telegram_message_id.is_none(),
            "payee-settled/category-open member's message_id must be cleared for category re-prompt"
        );
    }

    #[tokio::test]
    async fn confirm_fans_out_to_all_members_of_a_merchant() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.toml");
        let cfg = cfg_with_state(&path);

        let mut state = State::default();
        for ext in ["tx1", "tx2"] {
            state.add_pending_full(
                ext,
                "TFL TRAVEL CH",
                -250,
                "2026-06-01",
                Some("Transport for London".into()),
                Some("medium".into()),
                "",
            );
        }
        for p in &mut state.pending_reviews {
            p.telegram_message_id = Some(900);
        }

        let gk = crate::telegram::group_key("TFL TRAVEL CH");
        let channel = crate::pipeline::tests::FakeChannel::new(
            vec![Reply {
                entry_key: gk,
                action: ReplyAction::Confirm,
            }],
            77,
        );
        let applied = poll(&cfg, &mut state, &channel).await.unwrap();
        assert_eq!(applied, 1, "one group applied");
        assert!(
            state
                .pending_reviews
                .iter()
                .all(|p| p.resolve_to == "Transport for London"),
            "both members resolved"
        );
    }
}
