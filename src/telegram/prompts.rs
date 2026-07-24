use std::collections::BTreeMap;
use std::collections::BTreeSet;

use tracing::warn;

use crate::config::PAYEE_THRESHOLD;
use crate::matching::best_payee_match;
use crate::state::{PendingReview, PromptDimension, State};

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
                pr.prompted_dimension = None;
            }
        }
    }
}

/// The group's current open question: payee if any member's payee is unsettled,
/// else category if any member's category is open, else `None` (fully resolved).
fn group_question<'a>(members: impl Iterator<Item = &'a PendingReview>) -> Option<PromptDimension> {
    let mut any_category = false;
    for m in members {
        match m.open_dimension() {
            Some(PromptDimension::Payee) => return Some(PromptDimension::Payee),
            Some(PromptDimension::Category) => any_category = true,
            None => {}
        }
    }
    any_category.then_some(PromptDimension::Category)
}

/// One-line confirmation of what a fully-resolved group settled to, e.g.
/// "✅ M&S → Groceries" (empty category target → "uncategorised"). Echoed when a
/// resolved group's lingering keyboard is cleared. A resolved member always has
/// its payee settled, so `resolve_to` (or, for a create-new with no typed name,
/// the suggestion/raw merchant) is the payee.
fn resolved_summary(pr: &PendingReview) -> String {
    let payee = if pr.resolve_to.is_empty() {
        pr.suggestion
            .clone()
            .unwrap_or_else(|| pr.raw_merchant.clone())
    } else {
        pr.resolve_to.clone()
    };
    let category = if pr.resolve_category_to.is_empty() {
        "uncategorised"
    } else {
        pr.resolve_category_to.as_str()
    };
    format!("\u{2705} {payee} \u{2192} {category}")
}

/// Ensure every not-fully-resolved merchant group has a live Telegram prompt for
/// its current open dimension. Sends at most one prompt per group per call
/// (payee dimension before category). An unanswered prompt for the same
/// dimension is never re-sent (no-spam). Buckets by exact `raw_merchant`; a
/// reply fans out to the whole group via `group_key`. Called by both `run`
/// (after import) and `poll` (after applying replies).
pub async fn ensure_prompts(
    state: &mut State,
    channel: &dyn ConfirmChannel,
    payee_names: &[String],
    category_names: &[String],
    state_path: &std::path::Path,
) {
    let mut order: Vec<String> = Vec::new();
    let mut buckets: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, p) in state.pending_reviews.iter().enumerate() {
        if !buckets.contains_key(&p.raw_merchant) {
            order.push(p.raw_merchant.clone());
        }
        buckets.entry(p.raw_merchant.clone()).or_default().push(i);
    }
    for merchant in order {
        let members = buckets[&merchant].clone();
        let Some(q) = group_question(members.iter().map(|&i| &state.pending_reviews[i])) else {
            // Fully resolved. The terminal prompt (the category keyboard) would
            // otherwise linger in the chat and invite re-taps that re-resolve
            // the same entry. Clear it once, echoing the resolved payee →
            // category, and forget the message id so later ticks don't re-edit
            // and stale taps become no-ops. Import happens next run via
            // take_resolved (which has already removed it before run's own
            // ensure_prompts call, so only poll reaches this branch).
            if let Some(mid) = members
                .iter()
                .find_map(|&i| state.pending_reviews[i].telegram_message_id)
            {
                let text = resolved_summary(&state.pending_reviews[members[0]]);
                if let Err(e) = channel.edit_message_text(mid, &text).await {
                    warn!("telegram edit (clear resolved keyboard) failed: {e}");
                }
                for &i in &members {
                    state.pending_reviews[i].telegram_message_id = None;
                    state.pending_reviews[i].prompted_dimension = None;
                }
                if let Err(e) = state.save(state_path) {
                    warn!("failed to persist cleared prompt id: {e}");
                }
            }
            continue;
        };
        // Already a live prompt for this dimension? -> stay quiet.
        let has_live = members.iter().any(|&i| {
            let p = &state.pending_reviews[i];
            p.telegram_message_id.is_some() && p.prompted_dimension == Some(q)
        });
        if has_live {
            continue;
        }
        // A superseded prompt id (built for the other dimension) to tidy, if any.
        let stale_id = members
            .iter()
            .find_map(|&i| state.pending_reviews[i].telegram_message_id);
        // Rep whose OWN open dimension == q so build_prompt renders the right
        // flavor; defensive fallback to the first member.
        let rep = members
            .iter()
            .copied()
            .find(|&i| state.pending_reviews[i].open_dimension() == Some(q))
            .unwrap_or(members[0]);
        if let Some(old) = stale_id {
            let text = match q {
                PromptDimension::Category => "\u{2705} Payee set \u{2014} now categorising\u{2026}",
                PromptDimension::Payee => "\u{21aa}\u{fe0f} Re-asking below.",
            };
            if let Err(e) = channel.edit_message_text(old, text).await {
                warn!("telegram edit_message_text failed (stale keyboard left in chat): {e}");
            }
        }
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
                    state.pending_reviews[i].prompted_dimension = Some(q);
                }
                if let Err(e) = state.save(state_path) {
                    warn!("failed to persist prompt id (may re-prompt next run): {e}");
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
            prompted_dimension: None,
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
            prompted_dimension: None,
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
            prompted_dimension: None,
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
            prompted_dimension: None,
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
            prompted_dimension: None,
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
            prompted_dimension: None,
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
            prompted_dimension: None,
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
            prompted_dimension: None,
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
    async fn ensure_collapses_same_merchant_into_one_prompt() {
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
        ensure_prompts(&mut state, &channel, &payees, &[], &path).await;
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
            state
                .pending_reviews
                .iter()
                .all(|p| p.prompted_dimension == Some(PromptDimension::Payee)),
            "all members marked prompted for the payee dimension"
        );
        assert!(
            channel.sent.lock().unwrap()[0]
                .chat_text
                .contains("(3 txns)")
        );
    }

    #[tokio::test]
    async fn ensure_distinct_merchants_sends_two_prompts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.toml");
        let mut state = crate::state::State {
            pending_reviews: vec![held("tx1", "TFL TRAVEL CH"), held("tx2", "TESCO 1234")],
            ..Default::default()
        };
        let channel = FakeChannel::new(vec![], 0);
        let payees = vec!["Transport for London".to_string()];
        ensure_prompts(&mut state, &channel, &payees, &[], &path).await;
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
    async fn ensure_prompts_sends_once_then_stays_quiet() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.toml");
        let mut state = crate::state::State {
            pending_reviews: vec![held("tx1", "TFL TRAVEL CH")],
            ..Default::default()
        };
        let channel = FakeChannel::new(vec![], 0);
        let payees = vec!["Transport for London".to_string()];

        ensure_prompts(&mut state, &channel, &payees, &[], &path).await;
        assert_eq!(channel.sent.lock().unwrap().len(), 1, "prompted once");
        assert_eq!(
            state.pending_reviews[0].prompted_dimension,
            Some(crate::state::PromptDimension::Payee)
        );
        assert!(state.pending_reviews[0].telegram_message_id.is_some());

        // second pass: same open dimension already has a live prompt -> no resend
        ensure_prompts(&mut state, &channel, &payees, &[], &path).await;
        assert_eq!(
            channel.sent.lock().unwrap().len(),
            1,
            "no re-prompt (no-spam)"
        );
    }

    #[tokio::test]
    async fn ensure_prompts_advances_payee_to_category() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.toml");
        // payee just settled, category open, still carrying the answered PAYEE prompt id
        let mut e = held("tx1", "SQ *CAFE");
        e.resolve_to = "Local Cafe".into();
        e.category_suggestion = Some("Dining".into());
        e.category_confidence = Some("medium".into());
        e.category_resolved = false;
        e.telegram_message_id = Some(900);
        e.prompted_dimension = Some(crate::state::PromptDimension::Payee);
        let mut state = crate::state::State {
            pending_reviews: vec![e],
            ..Default::default()
        };
        let channel = FakeChannel::new(vec![], 0);
        let cats = vec!["Dining".to_string()];

        ensure_prompts(
            &mut state,
            &channel,
            &["Local Cafe".to_string()],
            &cats,
            &path,
        )
        .await;
        // superseded payee keyboard tidied, fresh category prompt sent
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
        assert_ne!(
            state.pending_reviews[0].telegram_message_id,
            Some(900),
            "adopted new id"
        );
    }

    #[tokio::test]
    async fn ensure_prompts_heals_legacy_entry_with_stale_id_and_none_dimension() {
        // Reproduces the stranded state: payee-open member with a stale message_id
        // and prompted_dimension = None (a pre-upgrade entry).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.toml");
        let mut e = held("tx1", "Google");
        e.suggestion = None; // "New merchant — which payee?" flavor
        e.telegram_message_id = Some(141);
        e.prompted_dimension = None;
        let mut state = crate::state::State {
            pending_reviews: vec![e],
            ..Default::default()
        };
        let channel = FakeChannel::new(vec![], 0);

        ensure_prompts(&mut state, &channel, &[], &[], &path).await;
        assert_eq!(
            channel.sent.lock().unwrap().len(),
            1,
            "legacy entry re-prompted"
        );
        assert_eq!(
            state.pending_reviews[0].prompted_dimension,
            Some(crate::state::PromptDimension::Payee)
        );
    }

    #[tokio::test]
    async fn ensure_prompts_clears_keyboard_and_echoes_when_resolved() {
        // Category just tapped: payee settled, category settled, still carrying
        // the answered CATEGORY prompt id -> the group is fully resolved. The
        // terminal keyboard must be cleared (edited) with an echo of the choice,
        // and the message id forgotten so stale re-taps become no-ops.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.toml");
        let mut e = held("tx1", "M&S");
        e.resolve_to = "M&S".into();
        e.resolve_category_to = "Groceries".into();
        e.category_resolved = true;
        e.telegram_message_id = Some(201);
        e.prompted_dimension = Some(crate::state::PromptDimension::Category);
        let mut state = crate::state::State {
            pending_reviews: vec![e],
            ..Default::default()
        };
        let channel = FakeChannel::new(vec![], 0);

        ensure_prompts(
            &mut state,
            &channel,
            &["M&S".to_string()],
            &["Groceries".to_string()],
            &path,
        )
        .await;

        let edited = channel.edited.lock().unwrap();
        assert_eq!(edited.len(), 1, "resolved keyboard edited exactly once");
        assert_eq!(edited[0].0, 201);
        assert_eq!(edited[0].1, "\u{2705} M&S \u{2192} Groceries");
        drop(edited);
        assert_eq!(
            channel.sent.lock().unwrap().len(),
            0,
            "nothing re-prompted on full resolution"
        );
        assert_eq!(state.pending_reviews[0].telegram_message_id, None);
        assert_eq!(state.pending_reviews[0].prompted_dimension, None);
    }

    #[tokio::test]
    async fn ensure_prompts_mixed_group_is_payee_phase_first() {
        // One member payee-settled+category-open, one payee-open: group question = payee.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.toml");
        let mut settled = held("txA", "MR FERNANDO");
        settled.resolve_to = "Santander Checking".into();
        settled.category_resolved = false;
        settled.telegram_message_id = Some(135);
        settled.prompted_dimension = Some(crate::state::PromptDimension::Payee);
        let open = held("txB", "MR FERNANDO"); // payee-open
        let mut state = crate::state::State {
            pending_reviews: vec![settled, open],
            ..Default::default()
        };
        let channel = FakeChannel::new(vec![], 0);
        let payees = vec!["Santander Checking".to_string()];

        ensure_prompts(&mut state, &channel, &payees, &[], &path).await;
        // group question is Payee (payee-open member exists); a live Payee prompt
        // already exists (id 135) -> no new send this pass.
        assert_eq!(channel.sent.lock().unwrap().len(), 0);
    }
}
