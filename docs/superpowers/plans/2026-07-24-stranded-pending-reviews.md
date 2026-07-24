# Stranded Pending Reviews — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Guarantee every not-fully-resolved pending review always has a live Telegram prompt for its current open dimension, so two-dimension (payee/category) reviews stop stranding in `state.toml` and previously-missing transactions import.

**Architecture:** Introduce a per-*group* prompt invariant. Each merchant group has one "current question" (payee-first, else category). A single shared `ensure_prompts` enforcer — called by both `poll` (after applying replies) and `run` (after import) — sends exactly one prompt per group per call when the open dimension has no matching live prompt, tracked by a new `prompted_dimension` field on `PendingReview`. The bespoke poll-side chaining (`chain_after_payee_settle`/`send_chained_category_prompts`) and `run`'s `send_group_prompts` are replaced by this one function. Payee-mutators become no-ops on already-settled members so a group-wide reply can't corrupt a settled sibling.

**Tech Stack:** Rust (async, `tokio`), `serde`/`toml` state, existing `ConfirmChannel` Telegram trait, inline `#[cfg(test)]` unit tests + `tokio::test`.

## Global Constraints

- **No state schema version bump.** The new field is additive with `#[serde(default)]`; `CURRENT_STATE_VERSION` stays `1`; old `state.toml` files load unchanged.
- **Clippy pedantic + nursery must pass:** `cargo clippy --all-targets -- -D warnings` (config lives in `Cargo.toml [lints.clippy]`). Never leave dead/unused functions — remove an orphaned function in the same task that orphans it.
- **Formatting gate:** finalize with `cargo fmt --all`; CI runs `cargo fmt --all -- --check`.
- **Tests:** `cargo test` (not `--lib`; run the whole suite).
- **Commits go through the Bash tool**, not PowerShell (PowerShell prepends a BOM to commit subjects on this machine). Commit messages contain no backticks/`$`/`!`.
- **Fail-loud invariant preserved:** nothing is silently dropped; anything unresolved stays held and gets a prompt.
- **Telegram prompts are plain text** (no Markdown parse_mode) — unchanged.
- Branch: `fix/stranded-pending-reviews` (already created; spec committed there).

---

## File Structure

- `src/state.rs` — add `PromptDimension` enum, `PendingReview.prompted_dimension` field, `PendingReview::open_dimension()`, guard payee-mutators. (Modify)
- `src/telegram/prompts.rs` — add `ensure_prompts` + `group_question`; delete `send_group_prompts`, `send_group_prompts_inner`, `send_chained_category_prompts`; clear `prompted_dimension` in `rehold_failed_remaps`. (Modify)
- `src/poll.rs` — call `ensure_prompts`; delete `chain_after_payee_settle` and the msg-id-clearing/`settled` block; rewrite tests. (Modify)
- `src/pipeline/mod.rs` — `run` calls `ensure_prompts` instead of `send_group_prompts`; add integration tests. (Modify)
- `src/pipeline/testutil.rs` — `held()` literal gains `prompted_dimension: None`. (Modify)

---

## Task 1: State — `PromptDimension`, field, `open_dimension()`

**Files:**
- Modify: `src/state.rs`
- Modify (compiler-guided): every `PendingReview { .. }` literal in `src/state.rs`, `src/poll.rs`, `src/telegram/prompts.rs`, `src/pipeline/mod.rs`, `src/pipeline/testutil.rs`
- Test: `src/state.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Produces: `pub enum PromptDimension { Payee, Category }` (Serialize/Deserialize, lowercase); `PendingReview.prompted_dimension: Option<PromptDimension>`; `PendingReview::open_dimension(&self) -> Option<PromptDimension>`.

- [ ] **Step 1: Add the enum**

In `src/state.rs`, near `PendingReview`:

```rust
/// Which review dimension a Telegram prompt is currently asking about.
/// Stored per group (shared across a merchant's members) so a run can tell an
/// unanswered prompt from one that asks the wrong (already-answered) dimension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptDimension {
    Payee,
    Category,
}
```

- [ ] **Step 2: Add the field to `PendingReview`**

Immediately after the `telegram_message_id` field:

```rust
    /// Dimension the live prompt (`telegram_message_id`) is asking about. `None`
    /// = never prompted (or a legacy entry) → the enforcer will (re)prompt.
    #[serde(default)]
    pub prompted_dimension: Option<PromptDimension>,
```

- [ ] **Step 3: Add `open_dimension()`**

In `impl PendingReview`, next to `payee_settled`:

```rust
    /// The dimension still needing a decision (payee before category), or `None`
    /// when both are settled (ready to import).
    #[must_use]
    pub fn open_dimension(&self) -> Option<PromptDimension> {
        if !self.payee_settled() {
            Some(PromptDimension::Payee)
        } else if !self.category_resolved {
            Some(PromptDimension::Category)
        } else {
            None
        }
    }
```

- [ ] **Step 4: Update the two struct-building constructors**

In `add_pending_full` and `add_pending_classified`, add to the `PendingReview { .. }` they push:

```rust
            prompted_dimension: None,
```

- [ ] **Step 5: Build; fix every remaining literal**

Run: `cargo build`
Expected: errors `missing field prompted_dimension in initializer of PendingReview`. Add `prompted_dimension: None,` to each flagged literal (state.rs tests, poll.rs tests, prompts.rs tests, pipeline/mod.rs tests, testutil.rs `held()`), until `cargo build` is clean.

- [ ] **Step 6: Write the roundtrip + open_dimension tests**

In `src/state.rs` tests:

```rust
#[test]
fn prompted_dimension_defaults_none_and_roundtrips() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("s.toml");
    // legacy file with NO prompted_dimension for the review -> loads as None
    std::fs::write(
        &path,
        "version = 1\n\n[[pending_review]]\nexternal_id = \"tx1\"\nraw_merchant = \"X\"\namount = -1\ndate = \"2026-06-01\"\n",
    )
    .unwrap();
    let mut s = State::load(&path).unwrap();
    assert_eq!(s.pending_reviews[0].prompted_dimension, None);

    s.pending_reviews[0].prompted_dimension = Some(PromptDimension::Category);
    s.save(&path).unwrap();
    let reloaded = State::load(&path).unwrap();
    assert_eq!(
        reloaded.pending_reviews[0].prompted_dimension,
        Some(PromptDimension::Category)
    );
}

#[test]
fn open_dimension_is_payee_then_category_then_none() {
    let mut p = PendingReview {
        external_id: "t".into(),
        source_monzo_id: MonzoAccountId::default(),
        raw_merchant: "X".into(),
        amount: -1,
        date: "2026-06-01".into(),
        resolve_to: String::new(),
        create_new: false,
        suggestion: None,
        confidence: None,
        category_suggestion: None,
        category_confidence: None,
        resolve_category_to: String::new(),
        category_resolved: false,
        telegram_message_id: None,
        prompted_dimension: None,
    };
    assert_eq!(p.open_dimension(), Some(PromptDimension::Payee));
    p.resolve_to = "Foo".into(); // payee settled
    assert_eq!(p.open_dimension(), Some(PromptDimension::Category));
    p.category_resolved = true; // both settled
    assert_eq!(p.open_dimension(), None);
}
```

- [ ] **Step 7: Run tests**

Run: `cargo test --quiet prompted_dimension open_dimension`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add src/state.rs src/poll.rs src/telegram/prompts.rs src/pipeline/mod.rs src/pipeline/testutil.rs
git commit -F - <<'EOF'
feat: add PromptDimension and open_dimension to PendingReview

Additive prompted_dimension field (serde default, no version bump) plus
an open_dimension() helper (payee-first, then category, then none). Wires
the field through all constructors and test fixtures.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
```

---

## Task 2: State — fan-out guards on payee mutators

**Files:**
- Modify: `src/state.rs` (`create_pending`, `remap_pending`, `confirm_pending`)
- Test: `src/state.rs` inline tests

**Interfaces:**
- Consumes: `PendingReview::payee_settled()` (existing).
- Produces: `create_pending`/`remap_pending`/`confirm_pending` are no-ops (return `false`) on an already-payee-settled member.

- [ ] **Step 1: Write the failing test**

In `src/state.rs` tests:

```rust
#[test]
fn payee_mutators_skip_already_settled_members() {
    let mut s = State::default();
    // settled to an existing payee (resolve_to set)
    s.pending_reviews.push(PendingReview {
        external_id: "settled".into(),
        source_monzo_id: MonzoAccountId::default(),
        raw_merchant: "X".into(),
        amount: -1,
        date: "2026-06-01".into(),
        resolve_to: "Santander Checking".into(),
        create_new: false,
        suggestion: Some("Something Else".into()),
        confidence: None,
        category_suggestion: None,
        category_confidence: None,
        resolve_category_to: String::new(),
        category_resolved: false,
        telegram_message_id: Some(135),
        prompted_dimension: Some(PromptDimension::Payee),
    });
    // create/remap/confirm must all no-op on a settled member
    assert!(!s.create_pending("settled"));
    assert!(!s.remap_pending("settled", "Wrong"));
    assert!(!s.confirm_pending("settled"));
    let p = &s.pending_reviews[0];
    assert!(!p.create_new, "create_new must stay false");
    assert_eq!(p.resolve_to, "Santander Checking", "resolve_to untouched");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --quiet payee_mutators_skip_already_settled_members`
Expected: FAIL (mutators currently act on settled members).

- [ ] **Step 3: Guard the three mutators**

In `create_pending`, before setting `create_new`:

```rust
        if let Some(p) = self
            .pending_reviews
            .iter_mut()
            .find(|p| p.external_id.as_str() == external_id)
        {
            if p.payee_settled() {
                return false; // never re-target an already-settled member
            }
            p.create_new = true;
            return true;
        }
        false
```

In `remap_pending`, same guard before setting `resolve_to`:

```rust
        if let Some(p) = self
            .pending_reviews
            .iter_mut()
            .find(|p| p.external_id.as_str() == external_id)
        {
            if p.payee_settled() {
                return false;
            }
            p.resolve_to = text.to_string();
            p.create_new = false;
            return true;
        }
        false
```

In `confirm_pending`, add the guard to the existing `if let` chain:

```rust
        if let Some(p) = self
            .pending_reviews
            .iter_mut()
            .find(|p| p.external_id.as_str() == external_id)
            && !p.payee_settled()
            && let Some(s) = p.suggestion.clone()
        {
            p.resolve_to = s;
            return true;
        }
        false
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --quiet payee_mutators_skip_already_settled_members reply_mutators_apply heterogeneous`
Expected: PASS (new guard test + existing `reply_mutators_apply` still green — its subjects are all unsettled).

- [ ] **Step 5: Commit**

```bash
git add src/state.rs
git commit -F - <<'EOF'
feat: no-op payee mutators on already-settled reviews

create_pending / remap_pending / confirm_pending now skip a member whose
payee is already settled, so a group-wide payee reply cannot corrupt a
settled sibling (e.g. flip Santander Checking to create_new).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
```

---

## Task 3: Prompts — `ensure_prompts` enforcer + `group_question`

**Files:**
- Modify: `src/telegram/prompts.rs` (add functions; keep existing ones for now)
- Test: `src/telegram/prompts.rs` inline tests

**Interfaces:**
- Consumes: `build_prompt` (existing), `PendingReview::open_dimension()` + `PromptDimension` (Task 1), `ConfirmChannel` (`send_prompt`, `edit_message_text`).
- Produces: `pub async fn ensure_prompts(state: &mut State, channel: &dyn ConfirmChannel, payee_names: &[String], category_names: &[String], state_path: &std::path::Path)`; private `fn group_question<'a>(members: impl Iterator<Item = &'a PendingReview>) -> Option<PromptDimension>`.

- [ ] **Step 1: Add imports**

Ensure `src/telegram/prompts.rs` imports include:

```rust
use crate::state::{PendingReview, PromptDimension, State};
```

- [ ] **Step 2: Write `group_question`**

```rust
/// The group's current open question: payee if any member's payee is unsettled,
/// else category if any member's category is open, else `None` (fully resolved).
fn group_question<'a>(
    members: impl Iterator<Item = &'a PendingReview>,
) -> Option<PromptDimension> {
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
```

- [ ] **Step 3: Write `ensure_prompts`**

```rust
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
            continue; // fully resolved -> imported by take_resolved on the next run
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
```

Note: the `edit`/`send` text uses unicode escapes to avoid mojibake from PowerShell editing; the rendered strings are "✅ Payee set — now categorising…" and "↪️ Re-asking below.".

- [ ] **Step 4: Write the unit tests**

In `src/telegram/prompts.rs` tests (helpers: `held(id, merchant)` = payee-open/category-resolved; `FakeChannel`):

```rust
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
    assert_eq!(channel.sent.lock().unwrap().len(), 1, "no re-prompt (no-spam)");
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

    ensure_prompts(&mut state, &channel, &["Local Cafe".to_string()], &cats, &path).await;
    // superseded payee keyboard tidied, fresh category prompt sent
    assert_eq!(channel.edited.lock().unwrap()[0].0, 900);
    assert_eq!(channel.sent.lock().unwrap().len(), 1);
    assert!(channel.sent.lock().unwrap()[0].chat_text.to_lowercase().contains("category"));
    assert_eq!(
        state.pending_reviews[0].prompted_dimension,
        Some(crate::state::PromptDimension::Category)
    );
    assert_ne!(state.pending_reviews[0].telegram_message_id, Some(900), "adopted new id");
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
    assert_eq!(channel.sent.lock().unwrap().len(), 1, "legacy entry re-prompted");
    assert_eq!(
        state.pending_reviews[0].prompted_dimension,
        Some(crate::state::PromptDimension::Payee)
    );
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
```

- [ ] **Step 5: Run tests**

Run: `cargo test --quiet ensure_prompts`
Expected: PASS (4 tests).

- [ ] **Step 6: Clippy + commit**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: clean (old functions still referenced by run/poll, so no dead-code yet).

```bash
git add src/telegram/prompts.rs
git commit -F - <<'EOF'
feat: add ensure_prompts enforcer and group_question

Single prompt-dispatch enforcer: one prompt per merchant group per call,
payee dimension before category, re-prompts only when the open dimension
has no matching live prompt. Not yet wired into run/poll.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
```

---

## Task 4: Wire `run` to `ensure_prompts`; clear dimension in `rehold_failed_remaps`

**Files:**
- Modify: `src/pipeline/mod.rs` (import + call site)
- Modify: `src/telegram/prompts.rs` (`rehold_failed_remaps`; delete `send_group_prompts`)
- Test: existing `src/pipeline/mod.rs` tests must stay green

**Interfaces:**
- Consumes: `ensure_prompts` (Task 3).
- Produces: `run` sends via `ensure_prompts`; `send_group_prompts` removed.

- [ ] **Step 1: Clear `prompted_dimension` when re-holding a failed remap**

In `rehold_failed_remaps` (`src/telegram/prompts.rs`), where it clears the message id, also clear the dimension:

```rust
                pr.suggestion = Some(std::mem::take(&mut pr.resolve_to));
                pr.telegram_message_id = None;
                pr.prompted_dimension = None;
```

- [ ] **Step 2: Switch the `run` import and call site**

In `src/pipeline/mod.rs`, change the import:

```rust
use crate::telegram::prompts::{ensure_prompts, rehold_failed_remaps};
```

and the send call (currently `send_group_prompts(state, ch, &payee_names, &category_names, &cfg.state_path).await;`):

```rust
    if let Some(ch) = channel {
        ensure_prompts(state, ch, &payee_names, &category_names, &cfg.state_path).await;
    }
```

- [ ] **Step 3: Delete `send_group_prompts`**

Remove the `pub async fn send_group_prompts(..)` wrapper from `src/telegram/prompts.rs` (its `send_group_prompts_inner` callee stays for now — still used by `send_chained_category_prompts`).

- [ ] **Step 4: Run the run-pipeline tests**

Run: `cargo test --quiet --test '*' ; cargo test --quiet pipeline::`
Expected: PASS. In particular `medium_match_held_prompted_then_imported_after_confirm`, `unconfirmed_hold_is_not_reprompted`, and `dry_run_sends_no_prompts_and_no_writes` still hold — a single medium-confidence hold gets one prompt, an unconfirmed hold is not re-prompted (open dim == prompted dim), and dry-run sends nothing.

If `unconfirmed_hold_is_not_reprompted` fails, verify the hold's `prompted_dimension` is set on the first run (it is, by `ensure_prompts`) so the second run sees a live matching prompt.

- [ ] **Step 5: Clippy + commit**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: clean.

```bash
git add src/pipeline/mod.rs src/telegram/prompts.rs
git commit -F - <<'EOF'
feat: run uses ensure_prompts for prompt dispatch

run() now drives all held-review prompting through the shared enforcer;
rehold_failed_remaps clears prompted_dimension so a re-held remap gets a
fresh prompt. Removes the old send_group_prompts wrapper.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
```

---

## Task 5: Wire `poll` to `ensure_prompts`; delete bespoke chaining

**Files:**
- Modify: `src/poll.rs` (call site; delete `chain_after_payee_settle` + the msg-id-clearing/`settled` block; rewrite tests)
- Modify: `src/telegram/prompts.rs` (delete `send_chained_category_prompts` and now-orphaned `send_group_prompts_inner`)

**Interfaces:**
- Consumes: `ensure_prompts` (Task 3).
- Produces: `poll` sends via `ensure_prompts`; `send_chained_category_prompts` and `send_group_prompts_inner` removed.

- [ ] **Step 1: Replace the post-apply block in `poll`**

In `src/poll.rs`, delete the `let mut settled ...` declaration, the whole `if any { if matches!(... Confirm|CreateNew|Remap ...) { ... } applied += 1; }` message-id-clearing block's inner id-clearing loop (keep `if any { applied += 1; }`), and the `chain_after_payee_settle(...)` call. Replace with a single enforcer call. The tail of `poll` becomes:

```rust
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
```

- [ ] **Step 2: Delete `chain_after_payee_settle`**

Remove the entire `async fn chain_after_payee_settle(..)` from `src/poll.rs`. Remove any imports it alone used (e.g. drop `BTreeMap`/`BTreeSet` only if the compiler flags them unused — `msg_map`/`valid` still use them, so likely keep both).

- [ ] **Step 3: Delete orphaned prompt functions**

In `src/telegram/prompts.rs`, remove `pub async fn send_chained_category_prompts(..)` and `async fn send_group_prompts_inner(..)` (now unused). Remove any imports left unused (e.g. `PAYEE_THRESHOLD`, `best_payee_match` are still used by `rehold_failed_remaps`; keep those).

- [ ] **Step 4: Rewrite the poll tests**

Delete these now-obsolete tests from `src/poll.rs`: `payee_confirm_chains_category_prompt_immediately`, `chaining_is_scoped_to_groups_settled_this_tick`, `empty_category_cache_falls_back_to_bare_clear`, `failed_keyboard_tidy_does_not_abort_chaining`. Keep and re-verify `category_confirm_settles_category_dimension`, `freetext_remap_targets_category_when_payee_settled`, `pick_category_resolves_hash_to_live_name`, `pick_category_unknown_hash_stays_held`, `applies_confirm_and_ignores_unknown_key`, `heterogeneous_group_payee_confirm_is_safe`, `confirm_fans_out_to_all_members_of_a_merchant`. Add one test proving poll chains payee→category same-tick via the enforcer:

```rust
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
    assert!(channel.sent.lock().unwrap()[0].chat_text.to_lowercase().contains("category"));
    assert_eq!(
        state.pending_reviews[0].prompted_dimension,
        Some(crate::state::PromptDimension::Category)
    );
}
```

- [ ] **Step 5: Run tests**

Run: `cargo test --quiet poll::`
Expected: PASS (kept tests + the new one).

- [ ] **Step 6: Clippy + commit**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: clean (all old prompt functions now removed).

```bash
git add src/poll.rs src/telegram/prompts.rs
git commit -F - <<'EOF'
feat: poll uses ensure_prompts; remove bespoke chaining

poll() applies replies then calls the shared enforcer, which chains the
category prompt same-tick after a payee settles. Deletes
chain_after_payee_settle, send_chained_category_prompts, and the orphaned
send_group_prompts_inner. Rewrites the chaining tests against the enforcer.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
```

---

## Task 6: Integration tests (strand + auto-heal) + final gate

**Files:**
- Test: `src/pipeline/mod.rs` inline tests (uses `run`, `FakeBank`, `fake_actual`, `FakeChannel`)

**Interfaces:**
- Consumes: `run`, `ensure_prompts` (via run/poll), test fixtures.

- [ ] **Step 1: Write the auto-heal integration test**

In `src/pipeline/mod.rs` tests — a legacy stranded entry (stale id, `prompted_dimension = None`) is re-prompted on the next run:

```rust
#[tokio::test]
async fn legacy_stranded_review_is_reprompted_on_next_run() {
    let shim = tempfile::tempdir().unwrap();
    let statedir = tempfile::tempdir().unwrap();
    let state_path = statedir.path().join("s.toml");
    let cfg = cfg_at(&state_path);
    let actual = fake_actual(&shim);
    let notifier = NullNotifier;
    let channel = FakeChannel::new(vec![], 0);

    let mut state = State::default();
    state.monzo.accounts.push(crate::state::AccountLink {
        monzo_id: "acc_1".into(),
        actual_id: Some(ActualAccountId::from("acc")),
        ..Default::default()
    });
    // Stranded: payee settled to an existing payee, category open, stale id,
    // NO prompted_dimension (pre-upgrade) -> must get a fresh category prompt.
    state.pending_reviews.push(PendingReview {
        external_id: "tx_stuck".into(),
        source_monzo_id: "acc_1".into(),
        raw_merchant: "Miruna Tecusanu".into(),
        amount: 12450,
        date: "2026-06-21".into(),
        resolve_to: "Local Cafe".into(),
        create_new: false,
        suggestion: None,
        confidence: None,
        category_suggestion: Some("Dining".into()),
        category_confidence: Some("medium".into()),
        resolve_category_to: String::new(),
        category_resolved: false,
        telegram_message_id: Some(145),
        prompted_dimension: None,
    });

    let mut bank = FakeBank { txs: vec![] };
    let claude = FakeClaude { out: vec![] };
    run(&cfg, false, &mut state, &mut bank, &actual, &claude, &notifier,
        Some(&channel as &dyn ConfirmChannel), Confidence::High).await.unwrap();

    assert_eq!(channel.sent.lock().unwrap().len(), 1, "stranded entry re-prompted");
    assert!(channel.sent.lock().unwrap()[0].chat_text.to_lowercase().contains("category"));
    assert_eq!(
        state.pending_reviews[0].prompted_dimension,
        Some(crate::state::PromptDimension::Category)
    );
}
```

- [ ] **Step 2: Write the end-to-end payee→category→import test**

```rust
#[tokio::test]
async fn two_dimension_review_resolves_across_runs_and_imports() {
    let shim = tempfile::tempdir().unwrap();
    let statedir = tempfile::tempdir().unwrap();
    let state_path = statedir.path().join("s.toml");
    let imports_log = shim.path().join("imports.log");
    let cfg = cfg_at(&state_path);
    let actual = fake_actual(&shim); // payees [Local Cafe]; cats [Groceries, Dining]
    let notifier = NullNotifier;
    let channel = FakeChannel::new(vec![], 0);

    let mut state = State::default();
    state.monzo.accounts.push(crate::state::AccountLink {
        monzo_id: "acc_1".into(),
        actual_id: Some(ActualAccountId::from("acc")),
        ..Default::default()
    });
    // Claude: payee Local Cafe HIGH (exists) -> pre-settled; category Dining LOW
    // (exists) -> uncertain -> held on the category dimension only.
    let mut bank = FakeBank { txs: vec![tx("tx_cafe", "SQ *CAFE", -450)] };
    let claude = FakeClaude {
        out: vec![cls("SQ *CAFE", Some("Local Cafe"), Confidence::High, Some("Dining"), Confidence::Low)],
    };
    run(&cfg, false, &mut state, &mut bank, &actual, &claude, &notifier,
        Some(&channel as &dyn ConfirmChannel), Confidence::High).await.unwrap();

    // Held on category; one category prompt sent; nothing imported yet.
    assert!(!imports_log.exists());
    assert_eq!(state.pending_reviews.len(), 1);
    assert_eq!(
        state.pending_reviews[0].prompted_dimension,
        Some(crate::state::PromptDimension::Category)
    );
    assert_eq!(channel.sent.lock().unwrap().len(), 1);

    // User confirms the category (what poll would do), then a run imports it.
    assert!(state.confirm_category("tx_cafe"));
    let mut bank2 = FakeBank { txs: vec![] };
    let claude2 = FakeClaude { out: vec![] };
    run(&cfg, false, &mut state, &mut bank2, &actual, &claude2, &notifier,
        Some(&channel as &dyn ConfirmChannel), Confidence::High).await.unwrap();

    let logged = std::fs::read_to_string(&imports_log).unwrap();
    assert!(logged.contains("tx_cafe"), "resolved two-dimension review imported");
    assert!(state.pending_reviews.is_empty(), "review cleared after import");
}
```

- [ ] **Step 3: Run the new integration tests**

Run: `cargo test --quiet legacy_stranded_review_is_reprompted_on_next_run two_dimension_review_resolves_across_runs_and_imports`
Expected: PASS.

- [ ] **Step 4: Full gate**

Run: `cargo test`
Expected: whole suite PASS.

Run: `cargo clippy --all-targets -- -D warnings`
Expected: clean.

Run: `cargo fmt --all`
Then: `cargo fmt --all -- --check`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add src/pipeline/mod.rs
git commit -F - <<'EOF'
test: integration cover for stranded-review heal and two-dimension flow

Auto-heal (legacy entry with no prompted_dimension gets re-prompted) and
end-to-end payee-presettled -> category-held -> confirm -> import.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
```

---

## Self-Review

**Spec coverage:**
- §1 per-group invariant, payee-first → Task 3 (`group_question`, `ensure_prompts`) + tests.
- §2 `prompted_dimension` field, no version bump → Task 1.
- §3 shared enforcer, both callers, delete bespoke chaining → Tasks 3–5.
- §4 fan-out guards → Task 2.
- §5 auto-heal (default `None`) → Task 6 (`legacy_stranded_review_is_reprompted_on_next_run`) + Task 3 unit.
- §6 error handling (warn-and-continue on send/edit/persist failure) → Task 3 `ensure_prompts` body.
- Testing section → Tasks 3, 5, 6.
- Rollout → out of code scope (operational).

**Placeholder scan:** No TBD/TODO; every code step has real code; every test has assertions.

**Type consistency:** `PromptDimension`, `open_dimension()`, `prompted_dimension` used identically across Tasks 1/3/5/6. `ensure_prompts` signature identical at definition (Task 3) and both call sites (Tasks 4, 5). `group_question` takes an iterator of `&PendingReview`, matching its call in `ensure_prompts`.

**Notes for the implementer:**
- Keep each task's build clippy-clean: functions are removed in the same task that orphans them (Task 4 removes `send_group_prompts`; Task 5 removes `send_chained_category_prompts` + `send_group_prompts_inner`).
- The rendered tidy strings contain an em-dash and emoji; they are written as `\u{..}` escapes to survive PowerShell edits on this machine.
