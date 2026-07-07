use std::collections::BTreeMap;
use std::collections::BTreeSet;

use tracing::warn;

use crate::config::PAYEE_THRESHOLD;
use crate::matching::best_payee_match;
use crate::state::{PendingReview, State};

use super::{ConfirmChannel, Prompt, encode_callback, group_key};

/// Build the Telegram prompt for a held entry, choosing the dimension to prompt:
/// - If the payee dimension is unsettled → payee prompt (3 flavors, unchanged):
///   - suggestion in payee list  -> "raw → suggestion?"  [Confirm][Create new][Skip]
///   - suggestion NOT in payees   -> "No payee 'x'. Create it?" [Create][Skip]
///   - no suggestion              -> "New merchant raw — which payee?" [Create new][Skip]
/// - If payee is settled but category is unsettled → category prompt (2 flavors):
///   - suggestion in category list -> "Category for raw → suggestion?" [Confirm][Skip]
///   - otherwise                   -> "Which category for raw?" [Skip]
///
/// The callback key is `group_key(raw_merchant)`; one prompt covers every held
/// transaction of that merchant, and one reply (button or free text) resolves all.
fn build_prompt(
    entry: &PendingReview,
    member_count: usize,
    payee_names: &[String],
    category_names: &[String],
) -> Prompt {
    let key_owned = group_key(&entry.raw_merchant);
    let key = key_owned.as_str();
    // ` (N txns)` only when the group has more than one held transaction.
    let count = if member_count > 1 {
        format!(" ({member_count} txns)")
    } else {
        String::new()
    };
    let row = |specs: &[(&str, &str)]| -> Vec<(String, String)> {
        specs
            .iter()
            .map(|(label, action)| {
                encode_callback(action, key).map(|data| (label.to_string(), data))
            })
            .collect::<Option<Vec<_>>>()
            .unwrap_or_default()
    };
    // One row holding a single owned-label button; empty row if encode fails.
    let one = |label: String, action: &str| -> Vec<(String, String)> {
        encode_callback(action, key)
            .map(|data| vec![(label, data)])
            .unwrap_or_default()
    };
    // The full category grid, 2 buttons per row, each `pc:<key>:<cat_hash>`.
    let category_grid = || -> Vec<Vec<(String, String)>> {
        let mut rows: Vec<Vec<(String, String)>> = Vec::new();
        let mut cur: Vec<(String, String)> = Vec::new();
        for name in category_names {
            let cat_hash = super::fnv1a_hex(name);
            if let Some(data) = encode_callback("pc", &format!("{key}:{cat_hash}")) {
                cur.push((name.clone(), data));
                if cur.len() == 2 {
                    rows.push(std::mem::take(&mut cur));
                }
            }
        }
        if !cur.is_empty() {
            rows.push(cur);
        }
        rows
    };

    // Payee dimension is settled when create_new is set or resolve_to is non-empty.
    let payee_settled = entry.payee_settled();
    if !payee_settled {
        // --- payee prompt (unchanged flavors) ---
        return match &entry.suggestion {
            Some(s) if payee_names.iter().any(|p| p == s) => {
                let conf = entry.confidence.as_deref().unwrap_or("medium");
                Prompt {
                    chat_text: format!(
                        "'{}'{} -> '{}'?  ({} confidence)\nReply with a different payee to remap.",
                        entry.raw_merchant, count, s, conf
                    ),
                    buttons: vec![row(&[
                        ("✅ Confirm", "cf"),
                        ("🆕 Create new", "nw"),
                        ("❌ Skip", "sk"),
                    ])],
                }
            }
            Some(s) => Prompt {
                chat_text: format!(
                    "No existing payee '{s}'{count}. Create it?\nOr reply with a payee name."
                ),
                buttons: vec![row(&[("🆕 Create", "nw"), ("❌ Skip", "sk")])],
            },
            None => Prompt {
                chat_text: format!(
                    "New merchant '{}'{} — which payee?\nReply with a payee name, or:",
                    entry.raw_merchant, count
                ),
                buttons: vec![row(&[("🆕 Create new", "nw"), ("❌ Skip", "sk")])],
            },
        };
    }

    // --- category prompt (payee already settled) ---
    let skip_row = one("❌ Skip (leave uncategorised)".to_string(), "xc");
    match &entry.category_suggestion {
        Some(s) if category_names.iter().any(|c| c == s) => {
            let conf = entry.category_confidence.as_deref().unwrap_or("medium");
            let mut rows = vec![one(format!("✅ Confirm: {s}"), "cc")];
            rows.extend(category_grid());
            rows.push(skip_row);
            Prompt {
                chat_text: format!(
                    "Category for '{}'{} -> '{}'?  ({} confidence)\n\
                     Tap a category below, or reply with a name.",
                    entry.raw_merchant, count, s, conf
                ),
                buttons: rows,
            }
        }
        // A suggestion not in the live list can't be confirmed (no "create
        // category" in Phase 2); drop the stale hint and show the grid.
        Some(_) | None => {
            let mut rows = category_grid();
            rows.push(skip_row);
            Prompt {
                chat_text: format!(
                    "Which category for '{}'{}?\nTap a category below, or reply with a name.",
                    entry.raw_merchant, count
                ),
                buttons: rows,
            }
        }
    }
}

/// Normalise free-text remap replies against the live payee list. A remap that
/// matches an existing payee is canonicalised; one that doesn't is re-held as a
/// "create it?" suggestion and re-prompted next run. The stale inline keyboard
/// is edited away first — once per group message, since a merchant's members
/// share one `telegram_message_id`.
pub async fn rehold_failed_remaps(
    state: &mut State,
    channel: Option<&dyn ConfirmChannel>,
    payee_names: &[String],
) {
    let mut edited: BTreeSet<i64> = BTreeSet::new();
    for pr in &mut state.pending_reviews {
        if !pr.create_new && !pr.resolve_to.is_empty() {
            if let Some(canon) = best_payee_match(&pr.resolve_to, payee_names, PAYEE_THRESHOLD) {
                pr.resolve_to = canon;
            } else {
                if let (Some(ch), Some(old_id)) = (channel, pr.telegram_message_id)
                    && edited.insert(old_id)
                {
                    let text = format!("↪️ Couldn't match '{}' — re-asking below.", pr.resolve_to);
                    if let Err(e) = ch.edit_message_text(old_id, &text).await {
                        warn!("telegram edit_message_text failed (stale prompt left in chat): {e}");
                    }
                }
                pr.suggestion = Some(std::mem::take(&mut pr.resolve_to));
                pr.telegram_message_id = None;
            }
        }
    }
}

/// Send one Telegram prompt per merchant group (exact `raw_merchant`). A reply
/// to that prompt resolves every held transaction of the merchant. A new
/// transaction joining a merchant that already has a live prompt adopts its
/// `message_id` instead of triggering a second prompt ("adopt, don't resend").
pub async fn send_group_prompts(
    state: &mut State,
    channel: &dyn ConfirmChannel,
    payee_names: &[String],
    category_names: &[String],
    state_path: &std::path::Path,
) {
    send_group_prompts_inner(
        state,
        channel,
        payee_names,
        category_names,
        state_path,
        None,
    )
    .await;
}

/// Poll-side variant: send category prompts ONLY for the given `raw_merchant`s
/// (the groups poll just settled this tick), using the cached Actual catalogs.
/// Same per-merchant send/adopt/persist semantics as [`send_group_prompts`].
pub async fn send_chained_category_prompts(
    state: &mut State,
    channel: &dyn ConfirmChannel,
    payee_names: &[String],
    category_names: &[String],
    state_path: &std::path::Path,
    only: &BTreeSet<String>,
) {
    send_group_prompts_inner(
        state,
        channel,
        payee_names,
        category_names,
        state_path,
        Some(only),
    )
    .await;
}

/// `restrict = Some(set)` sends only for merchants in `set`; `None` sends for
/// every held merchant (the `run()` path).
async fn send_group_prompts_inner(
    state: &mut State,
    channel: &dyn ConfirmChannel,
    payee_names: &[String],
    category_names: &[String],
    state_path: &std::path::Path,
    restrict: Option<&BTreeSet<String>>,
) {
    let mut order: Vec<String> = Vec::new();
    let mut buckets: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, p) in state.pending_reviews.iter().enumerate() {
        if !buckets.contains_key(&p.raw_merchant) {
            order.push(p.raw_merchant.clone());
        }
        buckets.entry(p.raw_merchant.clone()).or_default().push(i);
    }
    // Telegram rate limits: ~30 messages/sec globally and bursts to a single
    // chat are throttled (~1 msg/sec sustained). We send one prompt per held
    // merchant sequentially with no backoff; a run holding dozens of new
    // merchants at once could see a 429 (the send just warns and retries next
    // run — see the Err arm below). Acceptable at current volume; add pacing
    // here if held-merchant counts ever grow large.
    for merchant in order {
        // Poll's scoped path only re-prompts the groups it just settled; a
        // resolved sibling is impossible here (run() imports+removes resolved
        // entries each cycle and same-run same-merchant siblings share one
        // Claude classification), so the cleared-to-None members are the only
        // anchors and plain restriction is sufficient.
        if restrict.is_some_and(|only| !only.contains(&merchant)) {
            continue;
        }
        let members = buckets[&merchant].clone();
        if let Some(id) = members
            .iter()
            .find_map(|&i| state.pending_reviews[i].telegram_message_id)
        {
            for &i in &members {
                state.pending_reviews[i].telegram_message_id = Some(id);
            }
            continue;
        }
        // Prefer an unsettled-payee member so the prompt covers the payee dimension
        // first; fall back to the first member if every entry is already payee-settled.
        let rep = members
            .iter()
            .copied()
            .find(|&i| !state.pending_reviews[i].payee_settled())
            .unwrap_or(members[0]);
        let prompt = build_prompt(
            &state.pending_reviews[rep],
            members.len(),
            payee_names,
            category_names,
        );
        match channel.send_prompt(&prompt).await {
            Ok(message_id) => {
                for &i in &members {
                    state.pending_reviews[i].telegram_message_id = Some(message_id);
                }
                if let Err(e) = state.save(state_path) {
                    warn!("failed to persist telegram_message_id (may re-prompt next run): {e}");
                }
            }
            Err(e) => warn!("telegram send_prompt failed (will retry next run): {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{ExternalTxId, MonzoAccountId};
    use crate::pipeline::testutil::*;

    #[test]
    fn build_prompt_picks_flavor_by_payee_membership() {
        let payees = vec!["Tesco".to_string(), "Local Cafe".to_string()];

        // proposed, suggestion exists -> confirm/create/skip
        let proposed = PendingReview {
            external_id: "tx1".into(),
            source_monzo_id: MonzoAccountId::default(),
            raw_merchant: "SQ *CAFE".into(),
            amount: -450,
            date: "2026-06-01".into(),
            resolve_to: String::new(),
            create_new: false,
            suggestion: Some("Local Cafe".into()),
            confidence: Some("medium".into()),
            category_suggestion: None,
            category_confidence: None,
            resolve_category_to: String::new(),
            category_resolved: true,
            telegram_message_id: None,
        };
        let p = build_prompt(&proposed, 1, &payees, &[]);
        assert!(
            p.chat_text.contains("'Local Cafe'"),
            "expected plain-text quoting: {}",
            p.chat_text
        );
        assert!(
            !p.chat_text.contains("*Local Cafe*"),
            "must not use Markdown bold for payee: {}",
            p.chat_text
        );
        assert_eq!(p.buttons[0].len(), 3);
        assert_eq!(
            p.buttons[0][0].1,
            format!("cf:{}", crate::telegram::group_key(&proposed.raw_merchant))
        );

        // no-match -> create/skip only
        let unmatched = PendingReview {
            external_id: "tx2".into(),
            source_monzo_id: MonzoAccountId::default(),
            raw_merchant: "MYSTERY".into(),
            amount: -10,
            date: "2026-06-01".into(),
            resolve_to: String::new(),
            create_new: false,
            suggestion: None,
            confidence: None,
            category_suggestion: None,
            category_confidence: None,
            resolve_category_to: String::new(),
            category_resolved: true,
            telegram_message_id: None,
        };
        let p2 = build_prompt(&unmatched, 1, &payees, &[]);
        assert_eq!(p2.buttons[0].len(), 2);
        assert_eq!(
            p2.buttons[0][0].1,
            format!("nw:{}", crate::telegram::group_key(&unmatched.raw_merchant))
        );

        // failed remap: suggestion set but NOT in payees -> "create it?" flavor
        let failed = PendingReview {
            external_id: "tx3".into(),
            source_monzo_id: MonzoAccountId::default(),
            raw_merchant: "X".into(),
            amount: -10,
            date: "2026-06-01".into(),
            resolve_to: String::new(),
            create_new: false,
            suggestion: Some("Sainsbury's".into()),
            confidence: None,
            category_suggestion: None,
            category_confidence: None,
            resolve_category_to: String::new(),
            category_resolved: true,
            telegram_message_id: None,
        };
        let p3 = build_prompt(&failed, 1, &payees, &[]);
        assert!(p3.chat_text.contains("Sainsbury's"));
        assert_eq!(p3.buttons[0].len(), 2);
        assert_eq!(
            p3.buttons[0][0].1,
            format!("nw:{}", crate::telegram::group_key(&failed.raw_merchant))
        ); // create
        assert_eq!(
            p3.buttons[0][1].1,
            format!("sk:{}", crate::telegram::group_key(&failed.raw_merchant))
        ); // skip
    }

    #[test]
    fn build_prompt_keeps_buttons_regardless_of_external_id_length() {
        // The callback key is now group_key(raw_merchant) (16 hex chars), so even a
        // pathologically long external_id no longer drops the inline keyboard.
        let payees = vec!["Tesco".to_string()];
        let entry = PendingReview {
            external_id: ExternalTxId::from("z".repeat(200)),
            source_monzo_id: "acc_1".into(),
            raw_merchant: "SQ *CAFE".into(),
            amount: -100,
            date: "2026-06-01".into(),
            resolve_to: String::new(),
            create_new: false,
            suggestion: None,
            confidence: None,
            category_suggestion: None,
            category_confidence: None,
            resolve_category_to: String::new(),
            category_resolved: true,
            telegram_message_id: None,
        };
        let p = build_prompt(&entry, 1, &payees, &[]);
        assert_eq!(p.buttons[0].len(), 2, "create/skip buttons present");
        assert_eq!(
            p.buttons[0][0].1,
            format!("nw:{}", crate::telegram::group_key("SQ *CAFE"))
        );
    }

    #[test]
    fn build_prompt_category_flavor_when_payee_settled() {
        let payees = vec!["Local Cafe".to_string()];
        let cats = vec!["Dining".to_string(), "Groceries".to_string()];
        let entry = PendingReview {
            external_id: "tx1".into(),
            source_monzo_id: MonzoAccountId::default(),
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
            category_resolved: false, // category unsettled -> category prompt
            telegram_message_id: None,
        };
        let key = crate::telegram::group_key(&entry.raw_merchant);
        let p = build_prompt(&entry, 1, &payees, &cats);
        assert!(p.chat_text.to_lowercase().contains("category"));
        assert!(
            p.chat_text.contains("'Dining'"),
            "suggestion named: {}",
            p.chat_text
        );

        // First row: confirm-suggestion shortcut (cc). Last row: skip (xc).
        assert_eq!(p.buttons.first().unwrap()[0].1, format!("cc:{key}"));
        assert_eq!(p.buttons.last().unwrap()[0].1, format!("xc:{key}"));

        // Every live category appears as a pc:<key>:<hash> button somewhere in the grid.
        let all: Vec<&(String, String)> = p.buttons.iter().flatten().collect();
        for name in &cats {
            let want = format!("pc:{key}:{}", crate::telegram::fnv1a_hex(name));
            assert!(
                all.iter()
                    .any(|(label, data)| label == name && *data == want),
                "missing grid button for {name}: {all:?}"
            );
        }
    }

    #[test]
    fn build_prompt_category_no_suggestion_is_grid_plus_skip() {
        let payees = vec!["Local Cafe".to_string()];
        let cats = vec![
            "Dining".to_string(),
            "Groceries".to_string(),
            "Bills".to_string(),
        ];
        let entry = PendingReview {
            external_id: "tx9".into(),
            source_monzo_id: MonzoAccountId::default(),
            raw_merchant: "SQ *CAFE".into(),
            amount: -450,
            date: "2026-06-01".into(),
            resolve_to: "Local Cafe".into(),
            create_new: false,
            suggestion: None,
            confidence: None,
            category_suggestion: None, // no suggestion -> grid + skip only
            category_confidence: None,
            resolve_category_to: String::new(),
            category_resolved: false,
            telegram_message_id: None,
        };
        let key = crate::telegram::group_key(&entry.raw_merchant);
        let p = build_prompt(&entry, 1, &payees, &cats);
        // No confirm row: no button carries the cc action.
        assert!(
            !p.buttons
                .iter()
                .flatten()
                .any(|(_, d)| d.starts_with("cc:")),
            "no confirm shortcut without a suggestion"
        );
        // Skip is the last row.
        assert_eq!(p.buttons.last().unwrap()[0].1, format!("xc:{key}"));
        // 3 categories at 2/row -> 2 grid rows, + skip row = 3 rows.
        assert_eq!(p.buttons.len(), 3, "grid rows: {:?}", p.buttons);
    }

    #[test]
    fn build_prompt_payee_first_when_payee_unsettled() {
        let payees = vec!["Local Cafe".to_string()];
        let cats = vec!["Dining".to_string()];
        let entry = PendingReview {
            external_id: "tx2".into(),
            source_monzo_id: MonzoAccountId::default(),
            raw_merchant: "SQ *CAFE".into(),
            amount: -450,
            date: "2026-06-01".into(),
            resolve_to: String::new(),
            create_new: false,
            suggestion: Some("Local Cafe".into()),
            confidence: Some("medium".into()),
            category_suggestion: Some("Dining".into()),
            category_confidence: Some("low".into()),
            resolve_category_to: String::new(),
            category_resolved: false,
            telegram_message_id: None,
        };
        let p = build_prompt(&entry, 1, &payees, &cats);
        // payee prompt first (unchanged flavor): confirm/create/skip
        assert_eq!(p.buttons[0].len(), 3);
        assert_eq!(
            p.buttons[0][0].1,
            format!("cf:{}", crate::telegram::group_key(&entry.raw_merchant))
        );
    }

    #[test]
    fn build_prompt_shows_txn_count_when_grouped() {
        let payees = vec!["Transport for London".to_string()];
        let entry = PendingReview {
            external_id: "tx_a".into(),
            source_monzo_id: "acc_1".into(),
            raw_merchant: "TFL TRAVEL CH".into(),
            amount: -250,
            date: "2026-06-01".into(),
            resolve_to: String::new(),
            create_new: false,
            suggestion: Some("Transport for London".into()),
            confidence: Some("medium".into()),
            category_suggestion: None,
            category_confidence: None,
            resolve_category_to: String::new(),
            category_resolved: true,
            telegram_message_id: None,
        };
        let multi = build_prompt(&entry, 3, &payees, &[]);
        assert!(
            multi.chat_text.contains("(3 txns)"),
            "grouped count: {}",
            multi.chat_text
        );
        let single = build_prompt(&entry, 1, &payees, &[]);
        assert!(
            !single.chat_text.contains("txns"),
            "single member shows no count: {}",
            single.chat_text
        );
    }

    #[tokio::test]
    async fn send_collapses_same_merchant_into_one_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.toml");
        let mut state = crate::state::State {
            pending_reviews: vec![
                held("tx1", "TFL TRAVEL CH"),
                held("tx2", "TFL TRAVEL CH"),
                held("tx3", "TFL TRAVEL CH"),
            ],
            ..Default::default()
        };
        let channel = FakeChannel::new(vec![], 0);
        let payees = vec!["Transport for London".to_string()];
        send_group_prompts(&mut state, &channel, &payees, &[], &path).await;
        assert_eq!(
            channel.sent.lock().unwrap().len(),
            1,
            "one prompt for the merchant"
        );
        let ids: Vec<_> = state
            .pending_reviews
            .iter()
            .map(|p| p.telegram_message_id)
            .collect();
        assert!(
            ids.iter().all(Option::is_some) && ids[0] == ids[1] && ids[1] == ids[2],
            "all members share one message_id: {ids:?}"
        );
        assert!(
            channel.sent.lock().unwrap()[0]
                .chat_text
                .contains("(3 txns)")
        );
    }

    #[tokio::test]
    async fn send_distinct_merchants_sends_two_prompts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.toml");
        let mut state = crate::state::State {
            pending_reviews: vec![held("tx1", "TFL TRAVEL CH"), held("tx2", "TESCO 1234")],
            ..Default::default()
        };
        let channel = FakeChannel::new(vec![], 0);
        let payees = vec!["Transport for London".to_string()];
        send_group_prompts(&mut state, &channel, &payees, &[], &path).await;
        let sent = channel.sent.lock().unwrap();
        assert_eq!(sent.len(), 2);
        for p in sent.iter() {
            assert!(
                !p.chat_text.contains("txns"),
                "single-member prompt must show no count suffix: {}",
                p.chat_text
            );
        }
    }

    #[tokio::test]
    async fn send_adopts_existing_prompt_without_resending() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.toml");
        let mut a = held("tx1", "TFL TRAVEL CH");
        a.telegram_message_id = Some(777);
        let mut state = crate::state::State {
            pending_reviews: vec![a, held("tx2", "TFL TRAVEL CH")],
            ..Default::default()
        };
        let channel = FakeChannel::new(vec![], 0);
        let payees = vec!["Transport for London".to_string()];
        send_group_prompts(&mut state, &channel, &payees, &[], &path).await;
        assert_eq!(
            channel.sent.lock().unwrap().len(),
            0,
            "no resend for an outstanding group"
        );
        assert_eq!(
            state.pending_reviews[1].telegram_message_id,
            Some(777),
            "new member adopts the id"
        );
    }

    #[tokio::test]
    async fn send_chained_only_sends_for_listed_merchants() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.toml");

        // Two payee-settled, category-open groups, neither with a live prompt.
        let cat_open = |id: &str, merch: &str| PendingReview {
            external_id: id.into(),
            source_monzo_id: MonzoAccountId::default(),
            raw_merchant: merch.into(),
            amount: -100,
            date: "2026-06-01".into(),
            resolve_to: "Local Cafe".into(), // payee settled
            create_new: false,
            suggestion: None,
            confidence: None,
            category_suggestion: Some("Dining".into()),
            category_confidence: Some("medium".into()),
            resolve_category_to: String::new(),
            category_resolved: false, // category open
            telegram_message_id: None,
        };
        let mut state = crate::state::State {
            pending_reviews: vec![cat_open("a", "AAA"), cat_open("b", "BBB")],
            ..Default::default()
        };
        let channel = FakeChannel::new(vec![], 0);
        let cats = vec!["Dining".to_string()];
        let only: BTreeSet<String> = std::iter::once("AAA".to_string()).collect();

        send_chained_category_prompts(&mut state, &channel, &[], &cats, &path, &only).await;

        assert_eq!(channel.sent.lock().unwrap().len(), 1, "only AAA prompted");
        let a = state
            .pending_reviews
            .iter()
            .find(|p| p.raw_merchant == "AAA")
            .unwrap();
        let b = state
            .pending_reviews
            .iter()
            .find(|p| p.raw_merchant == "BBB")
            .unwrap();
        assert!(a.telegram_message_id.is_some(), "AAA got a prompt id");
        assert!(
            b.telegram_message_id.is_none(),
            "BBB excluded from the restrict set"
        );
    }
}
