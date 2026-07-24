//! `aba poll`: drain Telegram replies into the state file. Talks only to
//! Telegram, never to Actual — when a reply settles a payee with the category
//! still open, it chains the category prompt immediately (same tick) via the
//! shared `ensure_prompts` enforcer, using the Actual catalogs cached in state
//! by the last `run` (Phase D).

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
            .map(|p| p.external_id.as_str().to_owned())
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
        // Resolve a picked category hash to a live category name before the
        // member loop (which borrows `state` mutably). None => no live category
        // matches (e.g. deleted since the prompt) => leave held for re-prompt.
        let picked_category: Option<String> = match &r.action {
            ReplyAction::PickCategory(hash) => state
                .actual_category_names
                .iter()
                .find(|n| crate::telegram::fnv1a_hex(n) == *hash)
                .cloned(),
            _ => None,
        };

        let mut any = false;
        // `members` is collected before any mutation, so skip_pending's retain() is safe.
        for ext in &members {
            let ok = match &r.action {
                ReplyAction::Confirm => state.confirm_pending(ext),
                ReplyAction::CreateNew => state.create_pending(ext),
                ReplyAction::Skip => state.skip_pending(ext),
                ReplyAction::ConfirmCategory => state.confirm_category(ext),
                ReplyAction::SkipCategory => state.skip_category(ext),
                ReplyAction::PickCategory(_) => picked_category
                    .as_ref()
                    .is_some_and(|name| state.remap_category(ext, name)),
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
            applied += 1;
        }
    }

    // Ensure every still-open review has a live prompt for its current dimension,
    // using the Actual catalogs snapshotted by the last run().
    let payees = state.actual_payee_names.clone();
    let cats = state.actual_category_names.clone();
    crate::telegram::prompts::ensure_prompts(state, channel, &payees, &cats, &cfg.state_path).await;

    state.telegram.last_update_id = new_offset;
    state.save(&cfg.state_path)?;
    info!("poll applied {applied} group(s); offset now {new_offset}");
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{ExternalTxId, MonzoAccountId};
    use crate::pipeline::testutil::FakeChannel;
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

    fn payee_held(ext: &str, merchant: &str, msg_id: i64) -> crate::state::PendingReview {
        crate::state::PendingReview {
            external_id: ext.into(),
            source_monzo_id: "acc_1".into(),
            raw_merchant: merchant.into(),
            amount: -100,
            date: "2026-06-01".into(),
            resolve_to: String::new(),
            create_new: false,
            suggestion: Some(format!("{merchant} Payee")),
            confidence: Some("medium".into()),
            category_suggestion: None,
            category_confidence: None,
            resolve_category_to: String::new(),
            category_resolved: true, // category settled -> only payee dimension open
            telegram_message_id: Some(msg_id),
            prompted_dimension: Some(crate::state::PromptDimension::Payee),
        }
    }

    fn create_new_ready(ext: &str, merchant: &str, msg_id: i64) -> crate::state::PendingReview {
        crate::state::PendingReview {
            external_id: ext.into(),
            source_monzo_id: "acc_1".into(),
            raw_merchant: merchant.into(),
            amount: -100,
            date: "2026-06-01".into(),
            resolve_to: String::new(),
            create_new: false,
            suggestion: Some(merchant.into()), // create-new payee name
            confidence: Some("medium".into()),
            category_suggestion: None,
            category_confidence: None,
            resolve_category_to: "Groceries".into(),
            category_resolved: true, // category already settled -> a create tap fully resolves
            telegram_message_id: Some(msg_id),
            prompted_dimension: Some(crate::state::PromptDimension::Payee),
        }
    }

    #[tokio::test]
    async fn poll_clears_keyboard_for_every_group_resolved_in_one_batch() {
        // Bug: multiple "Create new" taps in one poll batch each resolve+import,
        // but only groups that CHAINED to a category prompt got their keyboard
        // cleared; fully-resolved groups left a dangling button. Both must clear.
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.toml");
        let cfg = cfg_with_state(&path);
        let mut state = State::default();
        state
            .pending_reviews
            .push(create_new_ready("txG", "GGG", 401));
        state
            .pending_reviews
            .push(create_new_ready("txH", "HHH", 402));

        let channel = FakeChannel::new(
            vec![
                Reply {
                    entry_key: crate::telegram::group_key("GGG"),
                    action: ReplyAction::CreateNew,
                },
                Reply {
                    entry_key: crate::telegram::group_key("HHH"),
                    action: ReplyAction::CreateNew,
                },
            ],
            88,
        );
        let applied = poll(&cfg, &mut state, &channel).await.unwrap();
        assert_eq!(applied, 2, "both create taps applied");

        // Both keyboards cleared (edited), echoing the resolved payee -> category.
        let edited = channel.edited.lock().unwrap();
        let ids: Vec<i64> = edited.iter().map(|(id, _)| *id).collect();
        assert!(
            ids.contains(&401) && ids.contains(&402),
            "both cleared: {ids:?}"
        );
        assert!(
            edited
                .iter()
                .any(|(_, t)| t == "\u{2705} GGG \u{2192} Groceries"),
            "echoes GGG: {edited:?}"
        );
        drop(edited);
        // Message ids forgotten so stale re-taps are inert.
        for m in ["GGG", "HHH"] {
            let pr = state
                .pending_reviews
                .iter()
                .find(|p| p.raw_merchant == m)
                .unwrap();
            assert_eq!(pr.telegram_message_id, None, "{m} id cleared");
            assert!(pr.create_new, "{m} still resolved (awaiting import)");
        }
    }

    #[tokio::test]
    async fn poll_applies_every_reply_in_a_multi_reply_batch() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.toml");
        let cfg = cfg_with_state(&path);
        let mut state = State::default();
        // Three distinct merchant groups, each with a live payee prompt.
        state.pending_reviews.push(payee_held("txA", "AAA", 301));
        state.pending_reviews.push(payee_held("txB", "BBB", 302));
        state.pending_reviews.push(payee_held("txC", "CCC", 303));

        // User tapped Confirm on all three before this single poll ran.
        let channel = FakeChannel::new(
            vec![
                Reply {
                    entry_key: crate::telegram::group_key("AAA"),
                    action: ReplyAction::Confirm,
                },
                Reply {
                    entry_key: crate::telegram::group_key("BBB"),
                    action: ReplyAction::Confirm,
                },
                Reply {
                    entry_key: crate::telegram::group_key("CCC"),
                    action: ReplyAction::Confirm,
                },
            ],
            77,
        );
        let applied = poll(&cfg, &mut state, &channel).await.unwrap();
        assert_eq!(applied, 3, "all three groups applied in one batch");
        for m in ["AAA", "BBB", "CCC"] {
            let pr = state
                .pending_reviews
                .iter()
                .find(|p| p.raw_merchant == m)
                .unwrap_or_else(|| panic!("{m} missing"));
            assert_eq!(
                pr.resolve_to,
                format!("{m} Payee"),
                "{m} payee should be settled"
            );
        }
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
            prompted_dimension: None,
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
            prompted_dimension: None,
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
    async fn pick_category_resolves_hash_to_live_name() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.toml");
        let cfg = cfg_with_state(&path);
        // Struct literal to avoid clippy::field_reassign_with_default.
        let mut state = State {
            actual_category_names: vec!["Dining".into(), "Groceries".into()],
            ..Default::default()
        };
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
            prompted_dimension: None,
        });
        let channel = FakeChannel::new(
            vec![Reply {
                entry_key: crate::telegram::group_key("SQ *CAFE"),
                action: ReplyAction::PickCategory(crate::telegram::fnv1a_hex("Groceries")),
            }],
            55,
        );
        let applied = poll(&cfg, &mut state, &channel).await.unwrap();
        assert_eq!(applied, 1);
        assert!(state.pending_reviews[0].category_resolved);
        assert_eq!(state.pending_reviews[0].resolve_category_to, "Groceries");
    }

    #[tokio::test]
    async fn pick_category_unknown_hash_stays_held() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.toml");
        let cfg = cfg_with_state(&path);
        // Struct literal to avoid clippy::field_reassign_with_default.
        let mut state = State {
            actual_category_names: vec!["Dining".into()], // "Groceries" was deleted
            ..Default::default()
        };
        state.pending_reviews.push(crate::state::PendingReview {
            external_id: "tx1".into(),
            source_monzo_id: "acc_1".into(),
            raw_merchant: "SQ *CAFE".into(),
            amount: -450,
            date: "2026-06-01".into(),
            resolve_to: "Local Cafe".into(),
            create_new: false,
            suggestion: None,
            confidence: None,
            category_suggestion: None,
            category_confidence: None,
            resolve_category_to: String::new(),
            category_resolved: false,
            telegram_message_id: Some(900),
            prompted_dimension: None,
        });
        let channel = FakeChannel::new(
            vec![Reply {
                entry_key: crate::telegram::group_key("SQ *CAFE"),
                action: ReplyAction::PickCategory(crate::telegram::fnv1a_hex("Groceries")),
            }],
            55,
        );
        let applied = poll(&cfg, &mut state, &channel).await.unwrap();
        assert_eq!(applied, 0, "unresolvable pick applies nothing");
        assert!(
            !state.pending_reviews[0].category_resolved,
            "stays held for re-prompt"
        );
    }

    #[tokio::test]
    async fn applies_confirm_and_ignores_unknown_key() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.toml");
        let cfg = cfg_with_state(&path);

        let mut state = State::default();
        state.add_pending_full(
            &ExternalTxId::from("tx1"),
            "SQ *CAFE",
            -450,
            "2026-06-01",
            Some("Local Cafe".into()),
            Some("medium".into()),
            &MonzoAccountId::default(),
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
            &ExternalTxId::from("txA"),
            "SQ *CAFE",
            -100,
            "2026-06-01",
            Some("Local Cafe".into()),
            Some("medium".into()),
            &MonzoAccountId::default(),
        );

        // Member B: same merchant but payee already settled, category open.
        // Must be a struct literal because add_pending_full cannot set resolve_to.
        state.pending_reviews.push(crate::state::PendingReview {
            external_id: "txB".into(),
            source_monzo_id: MonzoAccountId::default(),
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
            prompted_dimension: None,
        });

        for p in &mut state.pending_reviews {
            p.telegram_message_id = Some(910);
        }

        let gk = crate::telegram::group_key("SQ *CAFE");
        let channel = crate::pipeline::testutil::FakeChannel::new(
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
        // The whole group's question is now Category (both members payee-settled).
        // The enforcer tidies the stale payee keyboard (910) and fans a fresh
        // category prompt to every member same-tick — that is the heterogeneous-
        // group payoff: no waiting for the next run().
        assert_eq!(channel.edited.lock().unwrap()[0].0, 910);
        assert_eq!(
            state.pending_reviews[0].telegram_message_id,
            state.pending_reviews[1].telegram_message_id,
            "fresh prompt id fanned to every member of the group"
        );
        assert_ne!(
            state.pending_reviews[1].telegram_message_id,
            Some(910),
            "old payee prompt id superseded"
        );
        assert_eq!(
            state.pending_reviews[1].prompted_dimension,
            Some(crate::state::PromptDimension::Category)
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
                &ExternalTxId::from(ext),
                "TFL TRAVEL CH",
                -250,
                "2026-06-01",
                Some("Transport for London".into()),
                Some("medium".into()),
                &MonzoAccountId::default(),
            );
        }
        for p in &mut state.pending_reviews {
            p.telegram_message_id = Some(900);
        }

        let gk = crate::telegram::group_key("TFL TRAVEL CH");
        let channel = crate::pipeline::testutil::FakeChannel::new(
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

    #[tokio::test]
    async fn poll_confirm_chains_category_prompt_same_tick() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.toml");
        let cfg = cfg_with_state(&path);
        let mut state = State {
            actual_payee_names: vec!["Local Cafe".into()],
            actual_category_names: vec!["Dining".into()],
            ..Default::default()
        };
        // payee-open, category-open, live payee prompt id 900, prompted for payee
        state.pending_reviews.push(crate::state::PendingReview {
            external_id: "tx1".into(),
            source_monzo_id: "acc_1".into(),
            raw_merchant: "SQ *CAFE".into(),
            amount: -450,
            date: "2026-06-01".into(),
            resolve_to: String::new(),
            create_new: false,
            suggestion: Some("Local Cafe".into()),
            confidence: Some("medium".into()),
            category_suggestion: Some("Dining".into()),
            category_confidence: Some("medium".into()),
            resolve_category_to: String::new(),
            category_resolved: false,
            telegram_message_id: Some(900),
            prompted_dimension: Some(crate::state::PromptDimension::Payee),
        });
        let channel = FakeChannel::new(
            vec![Reply {
                entry_key: crate::telegram::group_key("SQ *CAFE"),
                action: ReplyAction::Confirm,
            }],
            55,
        );
        let applied = poll(&cfg, &mut state, &channel).await.unwrap();
        assert_eq!(applied, 1);
        assert_eq!(state.pending_reviews[0].resolve_to, "Local Cafe"); // payee settled
        // enforcer tidied the answered payee prompt (900) and sent a category prompt
        assert_eq!(channel.edited.lock().unwrap()[0].0, 900);
        assert_eq!(channel.sent.lock().unwrap().len(), 1);
        assert!(
            channel.sent.lock().unwrap()[0]
                .chat_text
                .to_lowercase()
                .contains("category")
        );
        assert_eq!(
            state.pending_reviews[0].prompted_dimension,
            Some(crate::state::PromptDimension::Category)
        );
    }
}
