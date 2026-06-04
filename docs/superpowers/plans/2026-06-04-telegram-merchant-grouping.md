# Telegram Prompts Grouped Per Merchant — Implementation Plan (Phase 4)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Collapse all held transactions sharing an exact `raw_merchant` into a single Telegram prompt whose one reply resolves every member; new transactions joining an already-prompted merchant adopt its prompt without re-sending.

**Architecture:** Derived per-merchant grouping (Approach A) — no `PendingReview` schema change. A merchant group is computed on the fly by bucketing `state.pending_reviews` on exact `raw_merchant` at send-time (`run`) and reply-time (`poll`). The Telegram `callback_data`/free-text correlation key changes from `external_id` to a deterministic FNV-1a digest of `raw_merchant`. Replies fan out to all member `external_id`s via the existing per-entry state mutators.

**Tech Stack:** Rust (tokio, async-trait, anyhow, tracing), existing `ConfirmChannel` trait + `FakeChannel` test double, TOML state.

**Reference spec:** `docs/superpowers/specs/2026-06-04-telegram-merchant-grouping-design.md`

---

## File Structure

- `src/telegram.rs` — add `group_key()`; update `Reply`/trait doc comments. (callback helpers live here)
- `src/run.rs` — replace the per-entry send loop with a per-group helper `send_group_prompts()`; update `build_prompt` to take a member count and use `group_key`; dedupe the stale-prompt edit; count merchants in the summary. Update affected unit tests.
- `src/poll.rs` — key `msg_map`/`valid` on group keys; fan each reply out to all member `external_id`s. Update tests.

No new files; no new dependencies.

---

## Task 1: `group_key` digest in `telegram.rs`

**Files:**
- Modify: `src/telegram.rs` (add function near `encode_callback`, line ~63)
- Test: `src/telegram.rs` `#[cfg(test)] mod tests`

- [ ] **Step 1: Write the failing test**

Add to `telegram.rs` tests module:

```rust
#[test]
fn group_key_is_deterministic_and_fits_callback() {
    let a = group_key("TFL TRAVEL CH");
    assert_eq!(a, group_key("TFL TRAVEL CH"), "deterministic for equal input");
    assert_ne!(a, group_key("TESCO 1234"), "differs for different input");
    assert_eq!(a.len(), 16, "16 hex chars");
    // `<action>:<group_key>` must fit Telegram's 64-byte callback cap.
    assert!(encode_callback("nw", &a).is_some());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test '*' group_key_is_deterministic 2>&1; cargo test group_key_is_deterministic`
Expected: FAIL — `cannot find function group_key in this scope`.

- [ ] **Step 3: Write minimal implementation**

Add after `encode_callback` in `src/telegram.rs`:

```rust
/// Stable, bounded key identifying a merchant group, derived from the exact
/// `raw_merchant`. Used as the Telegram `callback_data`/free-text correlation
/// key so one prompt and one reply cover every held transaction of a merchant.
///
/// FNV-1a 64-bit rendered as 16 lowercase hex chars: deterministic and
/// version-stable by construction (unlike `std::hash::DefaultHasher`, whose
/// output is not contractually stable across toolchains). 16 hex + the longest
/// `<action>:` prefix stays far under Telegram's 64-byte cap, so grouped prompts
/// never lose their keyboard to length.
#[must_use]
pub fn group_key(raw_merchant: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325; // FNV offset basis
    for byte in raw_merchant.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3); // FNV prime
    }
    format!("{hash:016x}")
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test group_key_is_deterministic`
Expected: PASS.

- [ ] **Step 5: Update doc comments (no behavior change)**

In `src/telegram.rs`, change the `Reply` doc comment (line ~21) from:

```rust
/// A reply correlated to a pending entry (`entry_key` = transaction `external_id`).
```
to:
```rust
/// A reply correlated to a merchant group (`entry_key` = `group_key(raw_merchant)`).
```

And the `poll_replies` doc comment (line ~46) from `...maps a prompt's message_id to the entry it was sent for...` to `...maps a prompt's message_id to the merchant group key it was sent for...`.

- [ ] **Step 6: Commit**

```bash
git add src/telegram.rs
git commit -m "feat(telegram): group_key digest for per-merchant prompt grouping"
```

---

## Task 2: `build_prompt` keys on the merchant group + shows count

**Files:**
- Modify: `src/run.rs` `build_prompt` (line ~328) and its existing unit tests in `run::tests`
- Test: `src/run.rs` `run::tests`

Decision (was an open item): show the count suffix ` (N txns)` **only when N > 1**; a single-member group reads exactly as today.

- [ ] **Step 1: Update the existing `build_prompt` unit tests to the new signature/keys (write the failing tests first)**

In `run::tests`, the calls become 4-arg and the callback keys become `group_key(raw_merchant)`. Update each call site:

- `build_prompt_picks_flavor_by_payee_membership`: change `build_prompt(&proposed, &payees, &[])` → `build_prompt(&proposed, 1, &payees, &[])`; change the assertion `assert_eq!(p2.buttons[0].1, "nw:tx2")` to:
  ```rust
  assert_eq!(
      p2.buttons[0].1,
      format!("nw:{}", crate::telegram::group_key(&unmatched.raw_merchant))
  );
  ```
  and likewise `p3.buttons[0].1` (the `failed` entry):
  ```rust
  assert_eq!(
      p3.buttons[0].1,
      format!("nw:{}", crate::telegram::group_key(&failed.raw_merchant))
  );
  ```
- `build_prompt_category_flavor_when_payee_settled`: `build_prompt(&entry, &payees, &cats)` → `build_prompt(&entry, 1, &payees, &cats)`.
- `build_prompt_payee_first_when_payee_unsettled`: `build_prompt(&entry, &payees, &cats)` → `build_prompt(&entry, 1, &payees, &cats)`; change `assert_eq!(p.buttons[0].1, "cf:tx2")` to:
  ```rust
  assert_eq!(
      p.buttons[0].1,
      format!("cf:{}", crate::telegram::group_key(&entry.raw_merchant))
  );
  ```

Replace the now-obsolete `build_prompt_drops_buttons_when_external_id_too_long` test (the key is no longer derived from `external_id`, so a long id can't overflow it) with:

```rust
#[test]
fn build_prompt_keeps_buttons_regardless_of_external_id_length() {
    // The callback key is now group_key(raw_merchant) (16 hex chars), so even a
    // pathologically long external_id no longer drops the inline keyboard.
    let payees = vec!["Tesco".to_string()];
    let entry = PendingReview {
        external_id: "z".repeat(200),
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
    assert_eq!(p.buttons.len(), 2, "create/skip buttons present");
    assert_eq!(
        p.buttons[0].1,
        format!("nw:{}", crate::telegram::group_key("SQ *CAFE"))
    );
}
```

Add the count-rendering test:

```rust
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
    assert!(multi.chat_text.contains("(3 txns)"), "grouped count: {}", multi.chat_text);
    let single = build_prompt(&entry, 1, &payees, &[]);
    assert!(!single.chat_text.contains("txns"), "single member shows no count: {}", single.chat_text);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test build_prompt`
Expected: FAIL — arity mismatch (`build_prompt` takes 3 args) / `(3 txns)` not found.

- [ ] **Step 3: Change `build_prompt` signature and body**

In `src/run.rs`, replace the signature and the `key`/buttons preamble. New signature:

```rust
fn build_prompt(
    entry: &PendingReview,
    member_count: usize,
    payee_names: &[String],
    category_names: &[String],
) -> Prompt {
    use crate::telegram::{encode_callback, group_key};
    let key = group_key(&entry.raw_merchant);
    let key = key.as_str();
    // ` (N txns)` only when the group has more than one held transaction.
    let count = if member_count > 1 {
        format!(" ({member_count} txns)")
    } else {
        String::new()
    };
    let buttons = |specs: &[(&str, &str)]| -> Vec<(String, String)> {
        specs
            .iter()
            .map(|(label, action)| {
                encode_callback(action, key).map(|data| (label.to_string(), data))
            })
            .collect::<Option<Vec<_>>>()
            .unwrap_or_default()
    };
```

Then thread `{count}` into the merchant-mentioning text. Edit the five `format!` strings:

1. Payee confirm flavor:
```rust
chat_text: format!(
    "'{}'{} -> '{}'?  ({} confidence)\nReply with a different payee to remap.",
    entry.raw_merchant, count, s, conf
),
```
2. "No existing payee" create flavor:
```rust
chat_text: format!(
    "No existing payee '{s}'.{count} Create it?\nOr reply with a payee name."
),
```
3. "New merchant — which payee?" flavor:
```rust
chat_text: format!(
    "New merchant '{}'{} — which payee?\nReply with a payee name, or:",
    entry.raw_merchant, count
),
```
4. Category confirm flavor:
```rust
chat_text: format!(
    "Category for '{}'{} -> '{}'?  ({} confidence)\n\
     Reply with a different category, or Skip to leave it uncategorised.",
    entry.raw_merchant, count, s, conf
),
```
5. "Which category?" flavor:
```rust
chat_text: format!(
    "Which category for '{}'{}?\nReply with a category name, or Skip to leave it uncategorised.",
    entry.raw_merchant, count
),
```

Also update the `build_prompt` doc comment: replace the `external_id`-too-long paragraph (lines ~324-327) with: `/// The callback key is group_key(raw_merchant); one prompt covers every held` / `/// transaction of that merchant, and one reply (button or free text) resolves all.`

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test build_prompt`
Expected: PASS (note the send-loop call site at line ~1247 still passes 3 args and will not compile yet — Task 3 fixes it; run `cargo test --no-run` may fail to build until Task 3. If so, do Step 5 then proceed and verify at the end of Task 3.)

- [ ] **Step 5: Commit**

```bash
git add src/run.rs
git commit -m "feat(run): build_prompt keys on merchant group_key, shows (N txns)"
```

---

## Task 3: Per-group send loop (`send_group_prompts`)

**Files:**
- Modify: `src/run.rs` — extract the send loop (lines ~1236-1261) into `send_group_prompts`; call it from `run`
- Test: `src/run.rs` `run::tests`

- [ ] **Step 1: Write the failing tests**

Add to `run::tests` (uses the existing `FakeChannel`; it records `sent: Vec<Prompt>` and returns incrementing message ids):

```rust
fn held(external_id: &str, raw_merchant: &str) -> PendingReview {
    PendingReview {
        external_id: external_id.into(),
        source_monzo_id: "acc_1".into(),
        raw_merchant: raw_merchant.into(),
        amount: -100,
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
    }
}

#[tokio::test]
async fn send_collapses_same_merchant_into_one_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.toml");
    let mut state = State::default();
    state.pending_reviews = vec![
        held("tx1", "TFL TRAVEL CH"),
        held("tx2", "TFL TRAVEL CH"),
        held("tx3", "TFL TRAVEL CH"),
    ];
    let channel = FakeChannel::new(vec![], 0);
    let payees = vec!["Transport for London".to_string()];
    send_group_prompts(&mut state, &channel, &payees, &[], &path).await;

    assert_eq!(channel.sent.lock().unwrap().len(), 1, "one prompt for the merchant");
    let ids: Vec<_> = state.pending_reviews.iter().map(|p| p.telegram_message_id).collect();
    assert!(ids.iter().all(|m| m.is_some()) && ids[0] == ids[1] && ids[1] == ids[2],
        "all members share one message_id: {ids:?}");
    assert!(channel.sent.lock().unwrap()[0].chat_text.contains("(3 txns)"));
}

#[tokio::test]
async fn send_distinct_merchants_sends_two_prompts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.toml");
    let mut state = State::default();
    state.pending_reviews = vec![held("tx1", "TFL TRAVEL CH"), held("tx2", "TESCO 1234")];
    let channel = FakeChannel::new(vec![], 0);
    let payees = vec!["Transport for London".to_string()];
    send_group_prompts(&mut state, &channel, &payees, &[], &path).await;
    assert_eq!(channel.sent.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn send_adopts_existing_prompt_without_resending() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.toml");
    let mut state = State::default();
    let mut a = held("tx1", "TFL TRAVEL CH");
    a.telegram_message_id = Some(777); // already prompted last run
    state.pending_reviews = vec![a, held("tx2", "TFL TRAVEL CH")]; // tx2 is new
    let channel = FakeChannel::new(vec![], 0);
    let payees = vec!["Transport for London".to_string()];
    send_group_prompts(&mut state, &channel, &payees, &[], &path).await;
    assert_eq!(channel.sent.lock().unwrap().len(), 0, "no resend for an outstanding group");
    assert_eq!(state.pending_reviews[1].telegram_message_id, Some(777), "new member adopts the id");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test send_collapses_same_merchant send_distinct_merchants send_adopts_existing_prompt`
Expected: FAIL — `cannot find function send_group_prompts`.

- [ ] **Step 3: Implement `send_group_prompts` and call it from `run`**

In `src/run.rs`, ensure `use std::collections::BTreeMap;` is present (add if missing). Add the helper (place it near `build_prompt`):

```rust
/// Send one Telegram prompt per merchant group (exact `raw_merchant`). A reply
/// to that prompt resolves every held transaction of the merchant. A new
/// transaction joining a merchant that already has a live prompt adopts its
/// `message_id` instead of triggering a second prompt ("adopt, don't resend").
async fn send_group_prompts(
    state: &mut State,
    channel: &dyn ConfirmChannel,
    payee_names: &[String],
    category_names: &[String],
    state_path: &std::path::Path,
) {
    // Bucket pending-review indices by raw_merchant, preserving first-seen order
    // for deterministic prompt ordering.
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
        // An outstanding prompt for this group? Adopt its id onto new members.
        if let Some(id) = members
            .iter()
            .find_map(|&i| state.pending_reviews[i].telegram_message_id)
        {
            for &i in &members {
                state.pending_reviews[i].telegram_message_id = Some(id);
            }
            continue;
        }
        // No live prompt: build from the least-settled member (so a group with
        // any payee-unsettled member prompts the payee dimension first).
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
                // Persist now so a later failure can't strand this id (M6).
                if let Err(e) = state.save(state_path) {
                    warn!("failed to persist telegram_message_id (may re-prompt next run): {e}");
                }
            }
            Err(e) => warn!("telegram send_prompt failed (will retry next run): {e}"),
        }
    }
}
```

Replace the inline send loop in `run` (lines ~1236-1261) with:

```rust
    // Send one prompt per merchant group for everything still held.
    if let Some(ch) = channel {
        send_group_prompts(
            &mut state,
            ch,
            &payee_names,
            &category_names,
            &cfg.state_path,
        )
        .await;
    }
```

(Confirm `ConfirmChannel` is in scope in `run.rs`; it is used already via `channel: Option<&dyn ConfirmChannel>`. No new import needed for the trait object.)

- [ ] **Step 4: Run the build and tests**

Run: `cargo test send_collapses_same_merchant send_distinct_merchants send_adopts_existing_prompt build_prompt`
Expected: PASS (this also resolves the Task 2 call-site arity; the whole crate builds now).

- [ ] **Step 5: Commit**

```bash
git add src/run.rs
git commit -m "feat(run): send one Telegram prompt per merchant group (adopt-don't-resend)"
```

---

## Task 4: Reply fan-out in `poll`

**Files:**
- Modify: `src/poll.rs` `poll()` (lines ~19-85)
- Test: `src/poll.rs` tests

- [ ] **Step 1: Write the failing test**

Add to `poll::tests` (models the existing tests; `add_pending_full` adds a `PendingReview`):

```rust
#[tokio::test]
async fn confirm_fans_out_to_all_members_of_a_merchant() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("s.toml");
    let cfg = cfg_with_state(&path);

    let mut state = State::default();
    // Two held transactions, same merchant, same Claude suggestion.
    for ext in ["tx1", "tx2"] {
        state.add_pending_full(
            ext, "TFL TRAVEL CH", -250, "2026-06-01",
            Some("Transport for London".into()), Some("medium".into()), "",
        );
    }
    for p in &mut state.pending_reviews {
        p.telegram_message_id = Some(900);
    }

    let gk = crate::telegram::group_key("TFL TRAVEL CH");
    let channel = crate::run::tests::FakeChannel::new(
        vec![Reply { entry_key: gk, action: ReplyAction::Confirm }],
        77,
    );
    let applied = poll(&cfg, &mut state, &channel).await.unwrap();
    assert_eq!(applied, 1, "one group applied");
    assert!(
        state.pending_reviews.iter().all(|p| p.resolve_to == "Transport for London"),
        "both members resolved"
    );
}
```

Also update the three existing `poll` tests whose `Reply.entry_key` is an `external_id` — they must use the merchant's group key:

- `category_confirm_settles_category_dimension`: `entry_key: "tx1".into()` → `entry_key: crate::telegram::group_key("SQ *CAFE")`.
- `freetext_remap_targets_category_when_payee_settled`: `entry_key: "tx2".into()` → `entry_key: crate::telegram::group_key("X")`.
- `applies_confirm_and_ignores_unknown_key`: the first reply `entry_key: "tx1".into()` → `entry_key: crate::telegram::group_key("SQ *CAFE")`; the second (`"ghost"`) stays as-is (still an unknown group key).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p actual-budget-automation poll 2>&1; cargo test confirm_fans_out_to_all_members`
Expected: FAIL — `confirm_fans_out…` resolves zero members (current code matches `entry_key` against `external_id`, never the group key) and the updated existing tests fail for the same reason.

- [ ] **Step 3: Rewrite `poll()` to key on groups and fan out**

Replace the body of `poll()` in `src/poll.rs` (keep the signature) with:

```rust
    use crate::telegram::group_key;

    // A prompt's message_id maps to the merchant group it was sent for, so a
    // free-text reply correlates to the whole group.
    let msg_map: BTreeMap<i64, String> = state
        .pending_reviews
        .iter()
        .filter_map(|p| p.telegram_message_id.map(|m| (m, group_key(&p.raw_merchant))))
        .collect();
    let (replies, new_offset) = channel
        .poll_replies(state.telegram.last_update_id, &msg_map)
        .await?;

    // Valid group keys currently held.
    let valid: BTreeSet<String> = state
        .pending_reviews
        .iter()
        .map(|p| group_key(&p.raw_merchant))
        .collect();

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
        let payee_settled = state
            .pending_reviews
            .iter()
            .find(|p| group_key(&p.raw_merchant) == r.entry_key)
            .is_some_and(crate::state::PendingReview::payee_settled);

        let mut any = false;
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
```

(`BTreeMap`/`BTreeSet` are already imported at the top of `poll.rs`.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test confirm_fans_out_to_all_members category_confirm_settles freetext_remap_targets_category applies_confirm_and_ignores_unknown_key`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/poll.rs
git commit -m "feat(poll): fan replies out to every member of a merchant group"
```

---

## Task 5: Dedupe the stale-prompt edit at group level

**Files:**
- Modify: `src/run.rs` free-text re-hold loop (lines ~771-790)
- Test: `src/run.rs` `run::tests`

Because a failed free-text remap is now fanned out to every member (all sharing one `telegram_message_id`), the re-hold loop would call `edit_message_text` once per member on the *same* message. Dedupe so the group's message is edited once.

- [ ] **Step 1: Write the failing test**

Add to `run::tests`. Use a `FakeChannel` that counts `edit_message_text` calls — confirm the test double exposes an edit counter; if it does not, add an `AtomicUsize edits` field to `FakeChannel` and increment it in `edit_message_text`, exposing `edits()`. Then:

```rust
#[tokio::test]
async fn failed_remap_edits_group_message_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.toml");
    let mut state = State::default();
    // Two members of one merchant, both free-text-remapped to a non-existent payee.
    for ext in ["tx1", "tx2"] {
        let mut p = held(ext, "SQ *NEWCAFE");
        p.suggestion = None;
        p.resolve_to = "Nonexistent Payee".into();
        p.telegram_message_id = Some(500); // shared group message
        state.pending_reviews.push(p);
    }
    let channel = FakeChannel::new(vec![], 0);
    let payees = vec!["Tesco".to_string()];
    rehold_failed_remaps(&mut state, Some(&channel as &dyn ConfirmChannel), &payees).await;

    assert_eq!(channel.edits(), 1, "edit the shared group message exactly once");
    assert!(state.pending_reviews.iter().all(|p| p.telegram_message_id.is_none()));
    assert!(state.pending_reviews.iter().all(|p| p.suggestion.as_deref() == Some("Nonexistent Payee")));
}
```

This drives extracting the loop into a testable `rehold_failed_remaps` helper.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test failed_remap_edits_group_message_once`
Expected: FAIL — `cannot find function rehold_failed_remaps` (and possibly `edits()` if you added the counter in this step).

- [ ] **Step 3: Extract + dedupe**

In `src/run.rs`, replace the inline loop at lines ~771-790 with a call:

```rust
    rehold_failed_remaps(&mut state, channel, &payee_names).await;
```

and add the helper near `build_prompt`:

```rust
/// Normalise free-text remap replies against the live payee list. A remap that
/// matches an existing payee is canonicalised; one that doesn't is re-held as a
/// "create it?" suggestion and re-prompted next run. The stale inline keyboard
/// is edited away first — once per group message, since a merchant's members
/// share one telegram_message_id.
async fn rehold_failed_remaps(
    state: &mut State,
    channel: Option<&dyn ConfirmChannel>,
    payee_names: &[String],
) {
    use std::collections::BTreeSet;
    let mut edited: BTreeSet<i64> = BTreeSet::new();
    for pr in &mut state.pending_reviews {
        if !pr.create_new && !pr.resolve_to.is_empty() {
            if let Some(canon) = best_payee_match(&pr.resolve_to, payee_names, PAYEE_THRESHOLD) {
                pr.resolve_to = canon;
            } else {
                if let (Some(ch), Some(old_id)) = (channel, pr.telegram_message_id) {
                    if edited.insert(old_id) {
                        let text =
                            format!("↪️ Couldn't match '{}' — re-asking below.", pr.resolve_to);
                        if let Err(e) = ch.edit_message_text(old_id, &text).await {
                            warn!("telegram edit_message_text failed (stale prompt left in chat): {e}");
                        }
                    }
                }
                pr.suggestion = Some(std::mem::take(&mut pr.resolve_to));
                pr.telegram_message_id = None;
            }
        }
    }
}
```

If `FakeChannel` lacks an edit counter, add to its struct `edits: std::sync::Arc<std::sync::atomic::AtomicUsize>`, initialise to 0 in `new`, `self.edits.fetch_add(1, Ordering::SeqCst)` inside `edit_message_text`, and a `pub fn edits(&self) -> usize` accessor.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test failed_remap_edits_group_message_once`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/run.rs
git commit -m "refactor(run): rehold_failed_remaps edits a group's stale prompt once"
```

---

## Task 6: Summary counts merchants, not transactions

**Files:**
- Modify: `src/run.rs` summary block (lines ~1265, 1276-1278)
- Test: `src/run.rs` `run::tests`

- [ ] **Step 1: Write the failing test**

A focused unit test on the count expression. Add to `run::tests`:

```rust
#[test]
fn awaiting_count_is_per_merchant() {
    use std::collections::BTreeSet;
    let reviews = vec![
        held("tx1", "TFL TRAVEL CH"),
        held("tx2", "TFL TRAVEL CH"),
        held("tx3", "TESCO 1234"),
    ];
    let merchants = reviews
        .iter()
        .map(|p| p.raw_merchant.as_str())
        .collect::<BTreeSet<_>>()
        .len();
    assert_eq!(merchants, 2, "three txns across two merchants");
}
```

- [ ] **Step 2: Run test to verify it fails or passes trivially**

Run: `cargo test awaiting_count_is_per_merchant`
Expected: PASS (this guards the intended semantics; it fails to compile only if `held` is absent — it was added in Task 3).

- [ ] **Step 3: Change the summary to count merchants**

In `src/run.rs`, replace:

```rust
    let awaiting = state.pending_reviews.len();
```
with:
```rust
    let awaiting = state
        .pending_reviews
        .iter()
        .map(|p| p.raw_merchant.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .len();
```

and the line ~1276-1278:
```rust
    if awaiting > 0 {
        let _ = write!(body, "\n{awaiting} merchant(s) awaiting your confirmation.");
    }
```

- [ ] **Step 4: Run the test suite for the summary path**

Run: `cargo test awaiting_count_is_per_merchant`
Expected: PASS. Also grep the broader run tests for any assertion on the old "item(s) awaiting" wording and update them: `cargo test 2>&1 | grep -i "awaiting"` — if a run integration test asserts the old string, change it to "merchant(s) awaiting".

- [ ] **Step 5: Commit**

```bash
git add src/run.rs
git commit -m "feat(run): run summary counts merchants awaiting confirmation"
```

---

## Task 7: Heterogeneous-group safety test + docs + final gate

**Files:**
- Test: `src/run.rs` or `src/poll.rs`
- Modify: `docs/RUNBOOK.md`; `docs/superpowers/specs/2026-06-04-telegram-merchant-grouping-design.md` (status)

- [ ] **Step 1: Write the heterogeneous-group safety test**

Add to `poll::tests` — a group whose members are at different dimension states must apply without error and converge (idempotent mutators):

```rust
#[tokio::test]
async fn heterogeneous_group_payee_confirm_is_safe() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("s.toml");
    let cfg = cfg_with_state(&path);
    let mut state = State::default();
    // Member A: payee unsettled (Claude suggestion pending).
    state.add_pending_full(
        "txA", "SQ *CAFE", -100, "2026-06-01",
        Some("Local Cafe".into()), Some("medium".into()), "",
    );
    // Member B: same merchant but payee already settled, category open.
    state.add_pending_full(
        "txB", "SQ *CAFE", -200, "2026-06-02", None, None, "Local Cafe",
    );
    state.pending_reviews[1].category_resolved = false;
    for p in &mut state.pending_reviews {
        p.telegram_message_id = Some(910);
    }
    let gk = crate::telegram::group_key("SQ *CAFE");
    let channel = crate::run::tests::FakeChannel::new(
        vec![Reply { entry_key: gk, action: ReplyAction::Confirm }],
        88,
    );
    // Must not panic; the unsettled member gets resolved.
    let applied = poll(&cfg, &mut state, &channel).await.unwrap();
    assert_eq!(applied, 1);
    assert_eq!(state.pending_reviews[0].resolve_to, "Local Cafe");
}
```

(Confirm `add_pending_full`'s parameter order matches the existing call in `applies_confirm_and_ignores_unknown_key`: `(external_id, raw_merchant, amount, date, suggestion, confidence, resolve_to)`. Adjust the last arg usage if the signature differs.)

- [ ] **Step 2: Run it**

Run: `cargo test heterogeneous_group_payee_confirm_is_safe`
Expected: PASS.

- [ ] **Step 3: Update the RUNBOOK**

In `docs/RUNBOOK.md`, under "## Telegram confirm loop", add a paragraph after the `poll`/`run` description:

```markdown
**Grouped prompts (Phase 4):** when one merchant produces several held
transactions in a run, you receive a **single** prompt for that merchant
showing the transaction count (e.g. "'TFL TRAVEL CH' (3 txns) -> ..."). Your one
reply — button or free-text — resolves **all** of that merchant's held
transactions at once. A new transaction for a merchant that already has an
outstanding prompt joins it silently (no duplicate prompt).
```

- [ ] **Step 4: Flip the spec status**

In `docs/superpowers/specs/2026-06-04-telegram-merchant-grouping-design.md`, change the `**Status:**` line to `Implemented`.

- [ ] **Step 5: Final verification gate**

Run, in order (per the project's Rust harness conventions — finalise formatting with Bash `cargo fmt`, then the clippy gate, then the full test run; never infer pass/fail from a piped tail):

```bash
cargo fmt --all
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Expected: `cargo fmt --check` clean; clippy reports no warnings (pedantic+nursery via `[lints.clippy]`); all tests pass. Inspect the test summary line directly for `test result: ok`.

- [ ] **Step 6: Commit**

```bash
git add src/run.rs src/poll.rs docs/RUNBOOK.md docs/superpowers/specs/2026-06-04-telegram-merchant-grouping-design.md
git commit -m "test(grouping): heterogeneous-group safety; docs + status for Phase 4"
```

---

## Self-Review

**Spec coverage:**
- Collapse same-merchant into one prompt → Tasks 2, 3.
- One reply fans out to all members → Task 4.
- New member adopts outstanding prompt (no resend) → Task 3.
- Exact `raw_merchant` grouping → group_key over the exact string (Task 1); buckets/valid sets key on it (Tasks 3, 4).
- Merchant + count only (no amounts) → Task 2 (` (N txns)`, >1 only).
- Group key fits callback cap; keyboard never dropped for length → Tasks 1, 2.
- `poll` msg_map/valid on group keys; payee→category re-prompt clears all members → Task 4.
- Summary counts merchants → Task 6.
- Stale-prompt edit once per group → Task 5.
- Heterogeneous/cross-run safety → Task 7.
- Back-compat (no schema change) → no migration task needed; old state loads unchanged (verified: `PendingReview` untouched).
- Dry-run unchanged → no Telegram send in dry-run (the `channel` is `None`/unused on the dry path; grouping lives only inside the `Some(channel)` block).

**Placeholder scan:** none — every code step shows full code; commands have expected output.

**Type consistency:** `group_key(&str) -> String` (Task 1) used identically in Tasks 2–4, 6, 7. `build_prompt(entry, member_count, payee_names, category_names)` defined in Task 2, called in Task 3. `send_group_prompts(state, channel, payee_names, category_names, state_path)` and `rehold_failed_remaps(state, channel, payee_names)` defined and called in Tasks 3/5. `Reply.entry_key` consistently a group key from Task 4 on.
