# Phase D — Poll-side Prompt Chaining Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let `aba poll` send the follow-up category prompt immediately after it settles a payee, instead of deferring it to the next `aba run` — collapsing the 2–3-run payee→category resolution to one cycle.

**Architecture:** `run()` snapshots Actual's live payee/category name catalogs into `State`. `poll()` reads those cached catalogs to build and send the category prompt for exactly the groups it just settled (scoped), tidying away the answered payee keyboard first. Poll still never touches Actual; bounded catalog staleness is re-validated at import.

**Tech Stack:** Rust, `async`/`tokio`, `serde` (TOML state), `async_trait` (Telegram `ConfirmChannel`), existing `FakeChannel`/`FakeBank`/`FakeClaude` test doubles.

**Spec:** `docs/superpowers/specs/2026-06-11-phase-d-poll-prompt-chaining-design.md`

---

## File Structure

| File | Responsibility | Change |
|------|----------------|--------|
| `src/state.rs` | State schema | +2 additive `Vec<String>` catalog fields + `Default` lines |
| `src/pipeline/mod.rs` | `run()` orchestration | snapshot catalogs into state; +1 integration test |
| `src/telegram/prompts.rs` | prompt building/sending | refactor `send_group_prompts` → `_inner` + new scoped `send_chained_category_prompts`; +1 test |
| `src/poll.rs` | reply draining | record settled groups → tidy → scoped category send; module doc; +4 tests |

Task order: **Task 1** (cache fields, independent) → **Task 2** (scoped sender, independent) → **Task 3** (poll wiring, depends on 1+2) → **Task 4** (gate + docs).

---

### Task 1: Cache the Actual name catalogs in `State`

**Files:**
- Modify: `src/state.rs` (the `State` struct ~lines 21–41 and its `Default` impl ~lines 43–56)
- Modify: `src/pipeline/mod.rs` (after `payee_names` is built, ~line 130)
- Test: `src/pipeline/mod.rs` tests module (after `medium_match_held_prompted_then_imported_after_confirm`, ~line 402)

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `src/pipeline/mod.rs`:

```rust
    #[tokio::test]
    async fn run_caches_actual_name_catalogs() {
        // run() must snapshot Actual's payee + category catalogs into State so
        // poll can build the chained category prompt without touching Actual.
        let shim = tempfile::tempdir().unwrap();
        let statedir = tempfile::tempdir().unwrap();
        let state_path = statedir.path().join("s.toml");
        let cfg = cfg_at(&state_path);
        let actual = fake_actual(&shim); // payees [Local Cafe]; categories [Groceries, Dining]
        let notifier = NullNotifier;
        let channel = FakeChannel::new(vec![], 0);

        let mut state = State::default();
        let mut bank = FakeBank { txs: vec![] };
        let claude = FakeClaude { out: vec![] };
        run(
            &cfg,
            false,
            &mut state,
            &mut bank,
            &actual,
            &claude,
            &notifier,
            Some(&channel as &dyn ConfirmChannel),
            Confidence::High,
        )
        .await
        .unwrap();

        assert_eq!(state.actual_payee_names, vec!["Local Cafe".to_string()]);
        assert_eq!(
            state.actual_category_names,
            vec!["Groceries".to_string(), "Dining".to_string()]
        );
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib run_caches_actual_name_catalogs`
Expected: FAIL — compile error `no field actual_payee_names on type State`.

- [ ] **Step 3: Add the two fields to `State`**

In `src/state.rs`, inside `pub struct State { ... }`, after the `category_cache` field (~line 33):

```rust
    /// Actual's live payee-name catalog, snapshotted at the last successful run.
    /// Lets `poll` build a chained category prompt without touching Actual.
    /// Distinct from `payee_cache` (a raw-merchant -> resolved-name map).
    #[serde(default)]
    pub actual_payee_names: Vec<String>,
    /// Actual's live category-name catalog, snapshotted at the last successful
    /// run. See [`State::actual_payee_names`].
    #[serde(default)]
    pub actual_category_names: Vec<String>,
```

In the `impl Default for State`, after `category_cache: BTreeMap::new(),` (~line 50):

```rust
            actual_payee_names: Vec::new(),
            actual_category_names: Vec::new(),
```

- [ ] **Step 4: Write the cache snapshot in `run()`**

In `src/pipeline/mod.rs`, immediately after the `let payee_names: Vec<String> = payees.iter().map(|p| p.name.clone()).collect();` line (~line 130):

```rust
    // Phase D: snapshot the live Actual catalogs so `poll` can build the chained
    // category prompt without touching Actual. Staleness is bounded by one run
    // and re-validated at import.
    state.actual_payee_names = payee_names.clone();
    state.actual_category_names = category_names.clone();
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test --lib run_caches_actual_name_catalogs`
Expected: PASS.

- [ ] **Step 6: Run the full suite (no regressions from the schema change)**

Run: `cargo test`
Expected: all pass (the `#[serde(default)]` fields are additive; existing state round-trip tests unaffected).

- [ ] **Step 7: Commit**

```bash
git add src/state.rs src/pipeline/mod.rs
git commit -m "feat(state): cache Actual payee/category catalogs in run() for poll chaining"
```

---

### Task 2: Add the scoped category sender to `prompts.rs`

**Files:**
- Modify: `src/telegram/prompts.rs` (`send_group_prompts`, ~lines 145–202)
- Test: `src/telegram/prompts.rs` tests module (after `send_adopts_existing_prompt_without_resending`, ~line 513)

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `src/telegram/prompts.rs`:

```rust
    #[tokio::test]
    async fn send_chained_only_sends_for_listed_merchants() {
        use std::collections::BTreeSet;
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
        let only: BTreeSet<String> = ["AAA".to_string()].into_iter().collect();

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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib send_chained_only_sends_for_listed_merchants`
Expected: FAIL — `cannot find function send_chained_category_prompts`.

- [ ] **Step 3: Refactor `send_group_prompts` into an inner fn + two wrappers**

In `src/telegram/prompts.rs`, replace the `pub async fn send_group_prompts(...) { <body> }` definition. Keep the existing body verbatim but move it into `send_group_prompts_inner` with an added `restrict` parameter and one guard line; add the two wrappers:

```rust
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
    send_group_prompts_inner(state, channel, payee_names, category_names, state_path, None).await;
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
    only: &std::collections::BTreeSet<String>,
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
    restrict: Option<&std::collections::BTreeSet<String>>,
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
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib send_chained_only_sends_for_listed_merchants`
Expected: PASS.

- [ ] **Step 5: Run the prompts tests (existing behavior preserved)**

Run: `cargo test --lib prompts::`
Expected: all pass — `send_group_prompts`'s public signature is unchanged, so `send_collapses_same_merchant_into_one_prompt`, `send_distinct_merchants_sends_two_prompts`, and `send_adopts_existing_prompt_without_resending` are unaffected.

- [ ] **Step 6: Commit**

```bash
git add src/telegram/prompts.rs
git commit -m "feat(telegram): add scoped send_chained_category_prompts for poll"
```

---

### Task 3: Wire poll to chain the category prompt

**Files:**
- Modify: `src/poll.rs` (import line ~6; the `if any { ... }` block ~lines 81–100; add chaining block before `state.telegram.last_update_id = ...` ~line 103)
- Test: `src/poll.rs` tests module (after `confirm_fans_out_to_all_members_of_a_merchant`, ~line 352)

- [ ] **Step 1: Write the failing tests**

Add all four to the `#[cfg(test)] mod tests` block in `src/poll.rs`:

```rust
    // Phase D: a payee-settling reply chains straight into the category prompt.
    fn payee_open_cat_open(id: &str, merch: &str, msg: Option<i64>) -> crate::state::PendingReview {
        crate::state::PendingReview {
            external_id: id.into(),
            source_monzo_id: "acc_1".into(),
            raw_merchant: merch.into(),
            amount: -450,
            date: "2026-06-01".into(),
            resolve_to: String::new(), // payee unsettled
            create_new: false,
            suggestion: Some("Local Cafe".into()),
            confidence: Some("medium".into()),
            category_suggestion: Some("Dining".into()),
            category_confidence: Some("medium".into()),
            resolve_category_to: String::new(),
            category_resolved: false, // category open
            telegram_message_id: msg,
        }
    }

    #[tokio::test]
    async fn payee_confirm_chains_category_prompt_immediately() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.toml");
        let cfg = cfg_with_state(&path);
        let mut state = State::default();
        state.actual_payee_names = vec!["Local Cafe".into()];
        state.actual_category_names = vec!["Dining".into()];
        state
            .pending_reviews
            .push(payee_open_cat_open("tx1", "SQ *CAFE", Some(900)));

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
        // answered payee keyboard tidied (old id 900)
        assert_eq!(channel.edits(), 1);
        assert_eq!(channel.edited.lock().unwrap()[0].0, 900);
        // a fresh category prompt was sent
        assert_eq!(channel.sent.lock().unwrap().len(), 1);
        assert!(
            channel.sent.lock().unwrap()[0]
                .chat_text
                .to_lowercase()
                .contains("category")
        );
        // member adopted the new id (FakeChannel hands out 1001 first), not 900
        assert_eq!(state.pending_reviews[0].telegram_message_id, Some(1001));
    }

    #[tokio::test]
    async fn chaining_is_scoped_to_groups_settled_this_tick() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.toml");
        let cfg = cfg_with_state(&path);
        let mut state = State::default();
        state.actual_payee_names = vec!["Local Cafe".into()];
        state.actual_category_names = vec!["Dining".into()];
        // Group A: payee unsettled, confirmed this tick.
        state
            .pending_reviews
            .push(payee_open_cat_open("txA", "AAA", Some(900)));
        // Group B: already payee-settled, category open, NO live prompt, NO reply
        // this tick (e.g. a prior run failed to send). Poll must NOT prompt it.
        state.pending_reviews.push(crate::state::PendingReview {
            external_id: "txB".into(),
            source_monzo_id: "acc_1".into(),
            raw_merchant: "BBB".into(),
            amount: -200,
            date: "2026-06-01".into(),
            resolve_to: "Local Cafe".into(),
            create_new: false,
            suggestion: None,
            confidence: None,
            category_suggestion: Some("Dining".into()),
            category_confidence: Some("medium".into()),
            resolve_category_to: String::new(),
            category_resolved: false,
            telegram_message_id: None,
        });

        let channel = FakeChannel::new(
            vec![Reply {
                entry_key: crate::telegram::group_key("AAA"),
                action: ReplyAction::Confirm,
            }],
            55,
        );
        poll(&cfg, &mut state, &channel).await.unwrap();

        assert_eq!(channel.sent.lock().unwrap().len(), 1, "only AAA prompted");
        let b = state
            .pending_reviews
            .iter()
            .find(|p| p.raw_merchant == "BBB")
            .unwrap();
        assert!(
            b.telegram_message_id.is_none(),
            "non-settled group B must stay unprompted"
        );
    }

    #[tokio::test]
    async fn empty_category_cache_falls_back_to_bare_clear() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.toml");
        let cfg = cfg_with_state(&path);
        let mut state = State::default();
        // actual_category_names deliberately left empty.
        state.actual_payee_names = vec!["Local Cafe".into()];
        state
            .pending_reviews
            .push(payee_open_cat_open("tx1", "SQ *CAFE", Some(900)));

        let channel = FakeChannel::new(
            vec![Reply {
                entry_key: crate::telegram::group_key("SQ *CAFE"),
                action: ReplyAction::Confirm,
            }],
            55,
        );
        let applied = poll(&cfg, &mut state, &channel).await.unwrap();

        assert_eq!(applied, 1);
        assert_eq!(state.pending_reviews[0].resolve_to, "Local Cafe"); // payee still settled
        assert_eq!(channel.sent.lock().unwrap().len(), 0, "no chained prompt");
        assert_eq!(channel.edits(), 0, "no keyboard tidy");
        assert!(
            state.pending_reviews[0].telegram_message_id.is_none(),
            "bare-cleared for next run to re-prompt"
        );
    }

    #[tokio::test]
    async fn failed_keyboard_tidy_does_not_abort_chaining() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.toml");
        let cfg = cfg_with_state(&path);
        let mut state = State::default();
        state.actual_payee_names = vec!["Local Cafe".into()];
        state.actual_category_names = vec!["Dining".into()];
        state
            .pending_reviews
            .push(payee_open_cat_open("tx1", "SQ *CAFE", Some(900)));

        let channel = FakeChannel::new(
            vec![Reply {
                entry_key: crate::telegram::group_key("SQ *CAFE"),
                action: ReplyAction::Confirm,
            }],
            55,
        )
        .with_failing_edit();
        let applied = poll(&cfg, &mut state, &channel).await.unwrap();

        assert_eq!(applied, 1);
        assert_eq!(
            channel.sent.lock().unwrap().len(),
            1,
            "category prompt still sent despite the failed tidy"
        );
        assert_eq!(state.pending_reviews[0].resolve_to, "Local Cafe");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib poll::tests::payee_confirm_chains_category_prompt_immediately`
Expected: FAIL — the chained prompt is not sent (`channel.sent` is empty; current code only nulls the id).

- [ ] **Step 3: Add the `warn` import**

In `src/poll.rs`, change the tracing import (~line 6):

```rust
use tracing::{info, warn};
```

- [ ] **Step 4: Record settled groups instead of bare-clearing**

In `src/poll.rs`, add a `settled` accumulator just after `let mut applied = 0usize;` (~line 40):

```rust
    // Phase D: groups whose payee we settle THIS tick (category still open),
    // keyed by raw_merchant -> the old prompt id (for keyboard tidy). Drives the
    // chained category send below.
    let mut settled: std::collections::BTreeMap<String, Option<i64>> =
        std::collections::BTreeMap::new();
```

Then replace the inner clearing loop inside `if any { ... }` (~lines 85–98) — change the body of the `for p in ...` loop:

```rust
                for p in state
                    .pending_reviews
                    .iter_mut()
                    .filter(|p| group_key(&p.raw_merchant) == r.entry_key)
                {
                    if p.payee_settled() && !p.category_resolved {
                        // Record (merchant, old prompt id) before clearing so we can
                        // tidy the answered payee keyboard and send a fresh category
                        // prompt below. Members share one id, so first-write wins.
                        settled
                            .entry(p.raw_merchant.clone())
                            .or_insert(p.telegram_message_id);
                        p.telegram_message_id = None;
                    }
                }
```

(`BTreeMap` is already imported at the top of `poll.rs`; the fully-qualified path above is harmless but you may use the bare `BTreeMap` name.)

- [ ] **Step 5: Add the chaining block after the reply loop**

In `src/poll.rs`, immediately before `state.telegram.last_update_id = new_offset;` (~line 103):

```rust
    // Phase D: chain the category prompt for groups whose payee we just settled,
    // using the cached Actual catalogs (poll never touches Actual). Skip when the
    // category cache is empty — fall back to run()'s next-cycle re-prompt. That is
    // effectively unreachable (a category dimension only opens after a run that
    // populated the cache) but guards a malformed/old state file.
    if !settled.is_empty() && !state.actual_category_names.is_empty() {
        // Strip the answered payee keyboard once per distinct old message id.
        let mut tidied: BTreeSet<i64> = BTreeSet::new();
        for old in settled.values().copied().flatten() {
            if tidied.insert(old) {
                if let Err(e) = channel
                    .edit_message_text(old, "✅ Payee set — now categorising…")
                    .await
                {
                    warn!(
                        "telegram edit_message_text failed (stale payee keyboard left in chat): {e}"
                    );
                }
            }
        }
        let merchants: BTreeSet<String> = settled.keys().cloned().collect();
        let payees = state.actual_payee_names.clone();
        let cats = state.actual_category_names.clone();
        crate::telegram::prompts::send_chained_category_prompts(
            state,
            channel,
            &payees,
            &cats,
            &cfg.state_path,
            &merchants,
        )
        .await;
    }
```

(`BTreeSet` is already imported at the top of `poll.rs`.)

- [ ] **Step 6: Run the new tests to verify they pass**

Run: `cargo test --lib poll::`
Expected: all four new tests PASS, plus the existing poll tests (`category_confirm_settles_category_dimension`, `freetext_remap_targets_category_when_payee_settled`, `applies_confirm_and_ignores_unknown_key`, `heterogeneous_group_payee_confirm_is_safe`, `confirm_fans_out_to_all_members_of_a_merchant`) still pass — none of them populate `actual_category_names`, so the chaining block is skipped (empty-cache guard), preserving their bare-clear expectations.

- [ ] **Step 7: Commit**

```bash
git add src/poll.rs
git commit -m "feat(poll): chain the category prompt immediately after payee settles"
```

---

### Task 4: Module doc, final gate, and memory

**Files:**
- Modify: `src/poll.rs` (module doc, line 1)
- Modify: memory `review-remediation-phases.md` (outside repo)

- [ ] **Step 1: Update the poll module doc**

In `src/poll.rs`, replace the first doc line:

```rust
//! `aba poll`: drain Telegram replies into the state file. Talks only to
//! Telegram, never to Actual — when a reply settles a payee with the category
//! still open, it chains the category prompt immediately using the Actual
//! catalogs cached in state by the last `run` (Phase D).
```

- [ ] **Step 2: Run `cargo fmt` (the Edit hook reorders imports case-insensitively; finalize formatting centrally)**

Run: `cargo fmt --all`
Then verify: `cargo fmt --all -- --check`
Expected: clean (exit 0).

- [ ] **Step 3: Run clippy at the CI gate level**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: no warnings. (Watch for `is_some_and`, `clippy::map_unwrap_or`, or needless-clone nits introduced in Tasks 2–3; remediate with `cargo clippy --fix --allow-dirty --all-targets` then re-verify.)

- [ ] **Step 4: Run the full test suite**

Run: `cargo test`
Expected: all pass (previous baseline 165 + 6 new = 171; confirm the printed `test result: ok` count rose by 6 and `0 failed`).

- [ ] **Step 5: Verify under `--no-default-features` (prod image build flavor)**

Run: `cargo test --no-default-features`
Expected: all pass (Phase A established a separate count under this flag; confirm `0 failed`).

- [ ] **Step 6: Commit the doc + any fmt/clippy fixups**

```bash
git add src/poll.rs
git commit -m "docs(poll): note Phase D category-prompt chaining in module doc"
```

- [ ] **Step 7: Update memory**

Update `C:\Users\Nando Ferreira\.claude\projects\G--rustDev-actual-budget-automation\memory\review-remediation-phases.md`: mark Phase D DONE on branch `refactor/phase-d-poll-prompt-chaining`, note the two new `State` catalog fields, `send_chained_category_prompts`, the scoped/tidy decisions, and the new test count. Refresh the description line.

---

## Self-Review

**Spec coverage:**
- Catalog cache (spec §1) → Task 1. ✓ (fields + run() write + test)
- Sender refactor + scoped entry point (spec §2) → Task 2. ✓
- Poll flow: tidy → clear → scoped send + empty-cache guard (spec §3) → Task 3. ✓
- "Never touches Actual" doc update (spec §3 invariant) → Task 4 Step 1. ✓
- Testing: chaining, scoping, empty-cache, best-effort tidy, run()-populates-cache (spec §4) → Tasks 1 & 3. ✓ (All five present. Note: a standalone "no Actual access" assertion is structurally guaranteed — `poll`'s signature has no `ActualClient` — so no explicit test is added, matching the spec's parenthetical.)
- No wire change / no schema bump (spec blast radius) → Task 1 uses `#[serde(default)]` additive fields; Task 4 Step 6 runs the full + `--no-default-features` suites to confirm round-trip. ✓

**Placeholder scan:** No TBD/TODO; every code step shows complete code; every command shows expected output. ✓

**Type/name consistency:**
- `actual_payee_names` / `actual_category_names` — identical across Tasks 1, 3, 4. ✓
- `send_chained_category_prompts(state, channel, payee_names, category_names, state_path, only)` — signature defined in Task 2 matches the call in Task 3 Step 5. ✓
- `send_group_prompts_inner(..., restrict: Option<&BTreeSet<String>>)` — defined and called only within Task 2. ✓
- `with_failing_edit()` / `channel.edits()` / `channel.edited` — real `FakeChannel` API (verified in testutil.rs). ✓
- `FakeChannel` first `send_prompt` returns `1001` (next_id starts 1000, pre-incremented) — matches Task 3 Step 1 assertion. ✓
- `payee_settled()` / `category_resolved` / `confirm_pending` — existing `PendingReview`/`State` API, unchanged. ✓
