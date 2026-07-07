# Telegram Category Picker Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the free-text "type a category name" Telegram prompt with an inline-button category picker (categories only; payee prompts unchanged).

**Architecture:** Category buttons carry `callback_data = "pc:<group_key>:<cat_hash>"` where `cat_hash` is the FNV-1a hash of the category name (same primitive as the existing `group_key`). A tap is drained on the next cron run and resolved by re-hashing the live `state.actual_category_names` list. `Prompt` gains a rows model (`Vec<Vec<(label, data)>>`) so the picker can render a multi-row grid.

**Tech Stack:** Rust, `serde_json`, `async_trait`, `tokio` tests, `wiremock`.

## Global Constraints

- Prompts are **plain text** — never set `parse_mode`; never use Markdown. (Existing invariant, enforced by tests.)
- Telegram `callback_data` must be **≤ 64 bytes** — `encode_callback` enforces this; a button whose data would exceed it is silently dropped. (`pc:` + 16 + `:` + 16 = 36 bytes, always safe.)
- Clippy runs **pedantic + nursery, `-D warnings`** in CI (`[lints.clippy]` in `Cargo.toml`). Remediate with `cargo clippy --fix --allow-dirty --all-targets`; the Edit hook may re-order imports.
- Finalize every task with `cargo fmt --all` (the CI gate includes `cargo fmt --all -- --check`).
- Run tests with **`cargo test`** (not `--lib`).
- On Windows, make git commits through the **Bash tool**, not PowerShell (PowerShell prepends a UTF-8 BOM to the commit subject). Commit messages here contain no backticks/`$`, so `-m` is safe.
- FNV-1a offset basis `0xcbf2_9ce4_8422_2325`, prime `0x0000_0100_0000_01b3`, rendered `{:016x}`. Do not change these constants — callback keys must stay stable across runs.

---

### Task 1: Extract the FNV-1a hex primitive and expose it

Generalize the merchant hash so category names can be hashed with the identical function.

**Files:**
- Modify: `src/telegram/mod.rs` (the `group_key` function, ~lines 91-99)
- Test: `src/telegram/mod.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Produces: `pub fn fnv1a_hex(s: &str) -> String` — 16 lowercase hex chars.
- Produces: `pub fn group_key(raw_merchant: &str) -> String` — unchanged behavior, now delegates to `fnv1a_hex`.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/telegram/mod.rs`:

```rust
#[test]
fn fnv1a_hex_matches_group_key_and_is_deterministic() {
    // group_key must be exactly fnv1a_hex of the same input (no behavior change).
    assert_eq!(group_key("TFL TRAVEL CH"), fnv1a_hex("TFL TRAVEL CH"));
    // deterministic + 16 hex chars
    assert_eq!(fnv1a_hex("Dining"), fnv1a_hex("Dining"));
    assert_eq!(fnv1a_hex("Dining").len(), 16);
    assert_ne!(fnv1a_hex("Dining"), fnv1a_hex("Groceries"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p actual-budget-automation fnv1a_hex_matches_group_key`
Expected: FAIL to compile — `cannot find function fnv1a_hex`.

- [ ] **Step 3: Add `fnv1a_hex` and delegate `group_key` to it**

In `src/telegram/mod.rs`, add above `group_key`:

```rust
/// FNV-1a 64-bit hash of `s`, rendered as 16 lowercase hex chars. Deterministic
/// and version-stable by construction (unlike `std::hash::DefaultHasher`).
/// Used for both merchant group keys and category callback keys.
#[must_use]
pub fn fnv1a_hex(s: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325; // FNV offset basis
    for byte in s.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3); // FNV prime
    }
    format!("{hash:016x}")
}
```

Replace the body of `group_key` (keep its existing doc comment) with:

```rust
#[must_use]
pub fn group_key(raw_merchant: &str) -> String {
    fnv1a_hex(raw_merchant)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p actual-budget-automation telegram::`
Expected: PASS (new test + all existing `group_key` tests green).

- [ ] **Step 5: Lint + format**

Run: `cargo clippy --all-targets -- -D warnings` then `cargo fmt --all`
Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
git add src/telegram/mod.rs
git commit -m "refactor: extract fnv1a_hex from group_key"
```

---

### Task 2: Change `Prompt` to a rows model (behavior-preserving)

Turn the flat single-row keyboard into a rows model so a picker can render a grid. This task keeps every existing prompt visually identical (each becomes exactly one row); only the type and the transport change.

**Files:**
- Modify: `src/telegram/mod.rs` — `struct Prompt` (~33-37), `build_send_request` (~211-225), and the `build_send_request` / direct-`Prompt` tests.
- Modify: `src/telegram/prompts.rs` — `build_prompt` (~23-108) and its tests.

**Interfaces:**
- Produces: `pub struct Prompt { pub chat_text: String, pub buttons: Vec<Vec<(String, String)>> }` — `buttons` is rows; each row is `(label, callback_data)` pairs.
- Consumes: `fnv1a_hex` (Task 1) — not used here yet, but `build_prompt` keeps compiling against Task 1's module.

- [ ] **Step 1: Change the `Prompt` struct**

In `src/telegram/mod.rs`, change the field type:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub chat_text: String,
    /// Inline-keyboard rows; each row is `(label, callback_data)` buttons.
    pub buttons: Vec<Vec<(String, String)>>,
}
```

- [ ] **Step 2: Update `build_send_request` to emit rows**

Replace the body of `build_send_request` (`src/telegram/mod.rs`) with:

```rust
pub fn build_send_request(prompt: &Prompt, chat_id: i64) -> Value {
    let mut body = json!({
        "chat_id": chat_id,
        "text": truncate_for_telegram(&prompt.chat_text),
    });
    if prompt.buttons.iter().any(|row| !row.is_empty()) {
        let rows: Vec<Value> = prompt
            .buttons
            .iter()
            .filter(|row| !row.is_empty())
            .map(|row| {
                Value::Array(
                    row.iter()
                        .map(|(label, data)| json!({ "text": label, "callback_data": data }))
                        .collect(),
                )
            })
            .collect();
        body["reply_markup"] = json!({ "inline_keyboard": rows });
    }
    body
}
```

- [ ] **Step 3: Update `build_prompt` to wrap existing buttons in one row each**

In `src/telegram/prompts.rs`, rename the `buttons` closure to `row` (unchanged body) and wrap each `Prompt`'s buttons in `vec![ ... ]`. The closure (lines ~37-45) stays:

```rust
    let row = |specs: &[(&str, &str)]| -> Vec<(String, String)> {
        specs
            .iter()
            .map(|(label, action)| {
                encode_callback(action, key).map(|data| (label.to_string(), data))
            })
            .collect::<Option<Vec<_>>>()
            .unwrap_or_default()
    };
```

Then in each of the four current `Prompt { ... buttons: buttons(&[...]) }` sites (the three payee flavors and the category `Some(_) | None` flavor), change `buttons: buttons(&[...])` to `buttons: vec![row(&[...])]`. For the category **suggestion-in-list** flavor (`Some(s) if ...`), change `buttons: buttons(&[("✅ Confirm", "cc"), ("❌ Skip", "xc")])` to `buttons: vec![row(&[("✅ Confirm", "cc"), ("❌ Skip", "xc")])]`.

(The full category rewrite happens in Task 4; here we only preserve behavior with the rows type.)

- [ ] **Step 4: Update direct-`Prompt` construction in `mod.rs` tests**

In `src/telegram/mod.rs` tests, the `build_send_request` tests construct `Prompt` directly. Update the two with buttons:

`build_send_request_has_text_and_buttons` (~464) — change its `buttons` to one row:

```rust
        let prompt = Prompt {
            chat_text: "hello".into(),
            buttons: vec![vec![
                ("✅ Confirm".into(), "cf:tx1".into()),
                ("❌ Skip".into(), "sk:tx1".into()),
            ]],
        };
```

The existing assertions (`kb = &body["reply_markup"]["inline_keyboard"][0]`, `kb[0]["callback_data"]`, `kb[1]["callback_data"]`) remain correct — `[0]` is the first row.

The three empty-button prompts (`buttons: vec![]` at ~450, ~614, ~680, ~706) already type-check as `Vec<Vec<_>>` and need no change.

- [ ] **Step 5: Add a multi-row transport test**

Add to `src/telegram/mod.rs` tests:

```rust
#[test]
fn build_send_request_emits_one_json_array_per_row() {
    let prompt = Prompt {
        chat_text: "grid".into(),
        buttons: vec![
            vec![("A".into(), "pc:g:a".into())],
            vec![("B".into(), "pc:g:b".into()), ("C".into(), "pc:g:c".into())],
        ],
    };
    let body = build_send_request(&prompt, 99);
    let kb = &body["reply_markup"]["inline_keyboard"];
    assert_eq!(kb.as_array().unwrap().len(), 2, "two rows");
    assert_eq!(kb[0].as_array().unwrap().len(), 1);
    assert_eq!(kb[1].as_array().unwrap().len(), 2);
    assert_eq!(kb[1][1]["callback_data"], "pc:g:c");
}
```

- [ ] **Step 6: Update `build_prompt` test assertions in `prompts.rs`**

In `src/telegram/prompts.rs` tests, every `p.buttons.len()` now counts **rows** and every `p.buttons[i].1` must become `p.buttons[0][i].1` (payee/category prompts here are still single-row after Task 2). Apply these edits:

- `build_prompt_picks_flavor_by_payee_membership`: `assert_eq!(p.buttons.len(), 3)` → `assert_eq!(p.buttons[0].len(), 3)`; `p.buttons[0].1` → `p.buttons[0][0].1`. For `p2`: `assert_eq!(p2.buttons.len(), 2)` → `assert_eq!(p2.buttons[0].len(), 2)`; `p2.buttons[0].1` → `p2.buttons[0][0].1`. For `p3`: `assert_eq!(p3.buttons.len(), 2)` → `assert_eq!(p3.buttons[0].len(), 2)`; `p3.buttons[0].1` → `p3.buttons[0][0].1`; `p3.buttons[1].1` → `p3.buttons[0][1].1`.
- `build_prompt_keeps_buttons_regardless_of_external_id_length`: `assert_eq!(p.buttons.len(), 2, ...)` → `assert_eq!(p.buttons[0].len(), 2, ...)`; `p.buttons[0].1` → `p.buttons[0][0].1`.
- `build_prompt_category_flavor_when_payee_settled`: this test is **replaced in Task 4** — leave it failing-to-compile-free for now by updating `p.buttons.len()`→`p.buttons[0].len()`, `p.buttons[0].1`→`p.buttons[0][0].1`, `p.buttons[1].1`→`p.buttons[0][1].1`. (Task 4 rewrites it.)
- `build_prompt_payee_first_when_payee_unsettled`: `assert_eq!(p.buttons.len(), 3)` → `assert_eq!(p.buttons[0].len(), 3)`; `p.buttons[0].1` → `p.buttons[0][0].1`.

- [ ] **Step 7: Run the full test suite**

Run: `cargo test -p actual-budget-automation`
Expected: PASS (all telegram + poll + pipeline tests green).

- [ ] **Step 8: Lint + format**

Run: `cargo clippy --all-targets -- -D warnings` then `cargo fmt --all`
Expected: no warnings.

- [ ] **Step 9: Commit**

```bash
git add src/telegram/mod.rs src/telegram/prompts.rs
git commit -m "refactor: Prompt carries inline-keyboard rows"
```

---

### Task 3: Add the `PickCategory` reply action and `pc` callback parsing

Teach the parser to decode `pc:<group_key>:<cat_hash>` into a reply carrying the chosen category's hash.

**Files:**
- Modify: `src/telegram/mod.rs` — `enum ReplyAction` (~12-22), add a `reply_from_callback` helper, and the callback branch of `parse_get_updates` (~166-183).
- Test: `src/telegram/mod.rs` tests.

**Interfaces:**
- Produces: `ReplyAction::PickCategory(String)` — the variant carries the 16-hex `cat_hash`, not the resolved name.
- Produces: `fn reply_from_callback(data: &str) -> Option<Reply>` — decodes both simple codes and `pc`.
- Consumes: `parse_callback` (unchanged), `action_from_code` (unchanged).

- [ ] **Step 1: Write the failing test**

Add to `src/telegram/mod.rs` tests:

```rust
#[test]
fn parse_get_updates_decodes_pick_category() {
    let group = group_key("SQ *CAFE");
    let cat = fnv1a_hex("Groceries");
    let data = format!("pc:{group}:{cat}");
    let result = serde_json::json!([
        { "update_id": 20, "callback_query": {
            "id": "cbp", "from": { "id": 99 },
            "message": { "chat": { "id": 99 } },
            "data": data
        }}
    ]);
    let parsed = parse_get_updates(&result, 99, &BTreeMap::new());
    assert_eq!(parsed.ack_callback_ids, vec!["cbp".to_string()]);
    assert_eq!(parsed.replies.len(), 1);
    assert_eq!(parsed.replies[0].entry_key, group);
    match &parsed.replies[0].action {
        ReplyAction::PickCategory(h) => assert_eq!(h, &cat),
        other => panic!("expected PickCategory, got {other:?}"),
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p actual-budget-automation parse_get_updates_decodes_pick_category`
Expected: FAIL to compile — no `ReplyAction::PickCategory`.

- [ ] **Step 3: Add the variant**

In `src/telegram/mod.rs`, add to `enum ReplyAction` (after `SkipCategory`):

```rust
    /// Pick a category by its callback hash (`fnv1a_hex(category_name)`);
    /// resolved to a live category name at reply-application time.
    PickCategory(String),
```

- [ ] **Step 4: Add `reply_from_callback` and wire it into `parse_get_updates`**

In `src/telegram/mod.rs`, add near `action_from_code`:

```rust
/// Decode a `callback_data` string into a `Reply`. Simple codes map to a
/// zero-payload action keyed by the whole remainder; `pc` carries a second
/// `:`-separated field: `pc:<group_key>:<cat_hash>`.
fn reply_from_callback(data: &str) -> Option<Reply> {
    let (code, key) = parse_callback(data)?;
    if code == "pc" {
        let (group, cat_hash) = key.split_once(':')?;
        if group.is_empty() || cat_hash.is_empty() {
            return None;
        }
        return Some(Reply {
            entry_key: group.to_string(),
            action: ReplyAction::PickCategory(cat_hash.to_string()),
        });
    }
    let action = action_from_code(&code)?;
    Some(Reply { entry_key: key, action })
}
```

In `parse_get_updates`, replace the callback decode block (currently `if let Some(data) = cq["data"].as_str() && let Some((code, key)) = parse_callback(data) && let Some(action) = action_from_code(&code) { replies.push(Reply { entry_key: key, action }); }`) with:

```rust
                if let Some(data) = cq["data"].as_str()
                    && let Some(reply) = reply_from_callback(data)
                {
                    replies.push(reply);
                }
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p actual-budget-automation telegram::`
Expected: PASS (new test + existing `parse_get_updates_*` tests, including `unknown_action_code_acks_but_yields_no_reply`, still green).

- [ ] **Step 6: Lint + format**

Run: `cargo clippy --all-targets -- -D warnings` then `cargo fmt --all`
Expected: no warnings.

- [ ] **Step 7: Commit**

```bash
git add src/telegram/mod.rs
git commit -m "feat: parse pc:<group>:<cat_hash> pick-category callbacks"
```

---

### Task 4: Build the category picker grid in `build_prompt`

Render the suggestion shortcut + full category grid + Skip instead of a free-text-only prompt.

**Files:**
- Modify: `src/telegram/prompts.rs` — the category branch of `build_prompt` (the `match &entry.category_suggestion { ... }` block, ~82-107) and its test.
- Test: `src/telegram/prompts.rs` tests.

**Interfaces:**
- Consumes: `fnv1a_hex` (Task 1), `encode_callback` (unchanged), the rows-typed `Prompt` (Task 2).
- Produces: category `Prompt`s whose `buttons` are `[confirm-row?] + [grid rows, 2/row] + [skip-row]`, each category button = `("<name>", "pc:<group_key>:<cat_hash>")`.

- [ ] **Step 1: Replace the `build_prompt_category_flavor_when_payee_settled` test**

In `src/telegram/prompts.rs`, replace that test body with assertions for the new grid:

```rust
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
        assert!(p.chat_text.contains("'Dining'"), "suggestion named: {}", p.chat_text);

        // First row: confirm-suggestion shortcut (cc). Last row: skip (xc).
        assert_eq!(p.buttons.first().unwrap()[0].1, format!("cc:{key}"));
        assert_eq!(p.buttons.last().unwrap()[0].1, format!("xc:{key}"));

        // Every live category appears as a pc:<key>:<hash> button somewhere in the grid.
        let all: Vec<&(String, String)> = p.buttons.iter().flatten().collect();
        for name in &cats {
            let want = format!("pc:{key}:{}", crate::telegram::fnv1a_hex(name));
            assert!(
                all.iter().any(|(label, data)| label == name && *data == want),
                "missing grid button for {name}: {all:?}"
            );
        }
    }

    #[test]
    fn build_prompt_category_no_suggestion_is_grid_plus_skip() {
        let payees = vec!["Local Cafe".to_string()];
        let cats = vec!["Dining".to_string(), "Groceries".to_string(), "Bills".to_string()];
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
            !p.buttons.iter().flatten().any(|(_, d)| d.starts_with("cc:")),
            "no confirm shortcut without a suggestion"
        );
        // Skip is the last row.
        assert_eq!(p.buttons.last().unwrap()[0].1, format!("xc:{key}"));
        // 3 categories at 2/row -> 2 grid rows, + skip row = 3 rows.
        assert_eq!(p.buttons.len(), 3, "grid rows: {:?}", p.buttons);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p actual-budget-automation build_prompt_category`
Expected: FAIL — old category branch produces a single Skip row, not the grid.

- [ ] **Step 3: Rewrite the category branch of `build_prompt`**

In `src/telegram/prompts.rs`, add two closures alongside `row` (after the `row` closure defined in Task 2):

```rust
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
```

Replace the whole category `match &entry.category_suggestion { ... }` block with:

```rust
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p actual-budget-automation build_prompt_category`
Expected: PASS (both category tests green).

- [ ] **Step 5: Run the full telegram module tests**

Run: `cargo test -p actual-budget-automation telegram::`
Expected: PASS.

- [ ] **Step 6: Lint + format**

Run: `cargo clippy --all-targets -- -D warnings` then `cargo fmt --all`
Expected: no warnings.

- [ ] **Step 7: Commit**

```bash
git add src/telegram/prompts.rs
git commit -m "feat: render category picker grid in Telegram prompt"
```

---

### Task 5: Resolve a picked category in `poll.rs`

Turn a `PickCategory(cat_hash)` reply into a category resolution by re-hashing the live catalog.

**Files:**
- Modify: `src/poll.rs` — the reply loop in `poll` (hoist a resolution before the member loop ~60-67; add a match arm ~72-85).
- Test: `src/poll.rs` tests.

**Interfaces:**
- Consumes: `ReplyAction::PickCategory(String)` (Task 3), `fnv1a_hex` (Task 1), `State::remap_category` (existing), `State::actual_category_names` (existing persisted field).
- Produces: nothing new; a resolved pick behaves exactly like a free-text category remap.

- [ ] **Step 1: Write the failing tests**

Add to `src/poll.rs` tests (near `freetext_remap_targets_category_when_payee_settled`):

```rust
    #[tokio::test]
    async fn pick_category_resolves_hash_to_live_name() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.toml");
        let cfg = cfg_with_state(&path);
        let mut state = State::default();
        state.actual_category_names = vec!["Dining".into(), "Groceries".into()];
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
        let mut state = State::default();
        state.actual_category_names = vec!["Dining".into()]; // "Groceries" was deleted
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
        assert!(!state.pending_reviews[0].category_resolved, "stays held for re-prompt");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p actual-budget-automation pick_category`
Expected: FAIL — `PickCategory` has no match arm (non-exhaustive match compile error).

- [ ] **Step 3: Hoist the pick resolution and add the match arm**

In `src/poll.rs`, immediately after the `let payee_settled = ...;` block (~63-67) and before `let mut any = false;`, add:

```rust
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
```

In the `match &r.action { ... }` inside the member loop, add an arm (after `ReplyAction::SkipCategory => ...`):

```rust
                ReplyAction::PickCategory(_) => match &picked_category {
                    Some(name) => state.remap_category(ext, name),
                    None => false,
                },
```

(No change to the `settled`/re-prompt block: `PickCategory` settles the category dimension, so it must stay out of the `Confirm | CreateNew | Remap(_)` re-prompt arm — leave that match as-is.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p actual-budget-automation pick_category`
Expected: PASS (both new tests green).

- [ ] **Step 5: Run the full suite**

Run: `cargo test -p actual-budget-automation`
Expected: PASS.

- [ ] **Step 6: Lint + format**

Run: `cargo clippy --all-targets -- -D warnings` then `cargo fmt --all`
Expected: no warnings.

- [ ] **Step 7: Commit**

```bash
git add src/poll.rs
git commit -m "feat: apply picked category by re-hashing live catalog"
```

---

### Task 6: Final verification

**Files:** none (verification only).

- [ ] **Step 1: Full build + test + lint + format-check**

Run:

```bash
cargo build --all-targets
cargo test -p actual-budget-automation
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: build OK, all tests pass, no clippy warnings, formatting clean. If `fmt --check` fails, run `cargo fmt --all` and amend the last commit.

- [ ] **Step 2: Manual sanity of the wire shape (optional but recommended)**

Confirm a category `Prompt` serializes to multiple `inline_keyboard` rows and every category button's `callback_data` is `pc:<16hex>:<16hex>` (≤ 64 bytes). This is already covered by `build_send_request_emits_one_json_array_per_row` and `build_prompt_category_flavor_when_payee_settled`; re-read those assertions to confirm they exercise the real shape.

---

## Self-Review

**Spec coverage:**
- Flat 2/row grid → Task 4 (`category_grid`). ✓
- Suggestion-first shortcut + grid + Skip → Task 4 (suggestion-in-list arm). ✓
- No-suggestion grid + Skip → Task 4 (`Some(_) | None` arm). ✓
- Free-text fallback retained → untouched `ReplyAction::Remap` path in `poll.rs` (Task 5 leaves it intact). ✓
- `pc:<group_key>:<cat_hash>` encoding, name-hash not index → Tasks 1, 3, 4. ✓
- Re-hash live list to resolve; deleted category stays held → Task 5 (both tests). ✓
- `Prompt` rows model + transport → Task 2. ✓
- `PickCategory` action + `pc` code + parse → Task 3. ✓
- Cron unchanged → no deployment task needed; the pull/offset drain is untouched. ✓
- Known limitation (duplicate names resolve by-name to first match) → inherent to `remap_category(name)`; no task changes it, matches spec "out of scope". ✓

**Placeholder scan:** No TBD/TODO; every code step shows full code. ✓

**Type consistency:** `fnv1a_hex(&str) -> String` (Task 1) used identically in Tasks 3/4/5. `PickCategory(String)` carries the hash in Task 3, resolved in Task 5. `Prompt.buttons: Vec<Vec<(String, String)>>` (Task 2) consumed in Tasks 2/4 and by `build_send_request`. `remap_category(&mut self, external_id: &str, name: &str) -> bool` matches the existing signature used in Task 5. ✓
