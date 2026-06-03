# Categories (Phase 2) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Claude classifies each merchant into one of the user's existing Actual categories in the same call as the payee; confident classifications auto-import with the category set, uncertain ones hold for sequential Telegram review (payee then category).

**Architecture:** Extends the existing payee pipeline with a second, parallel "category" dimension. Each held transaction now has two independently-settled dimensions (payee, category); it imports only when both are settled. A transaction auto-imports only when *both* payee and category are confident and resolve to live Actual entities; otherwise it is held with whichever dimension(s) are uncertain flagged for review.

**Tech Stack:** Rust (async, `anyhow`, `serde`, `async-trait`, `reqwest`, `wiremock`/`tokio` tests); a Node `@actual-app/api` shim driven over a one-message JSON stdin/stdout protocol.

---

## Verified contract decisions (read before starting)

These were verified against the **installed** `@actual-app/api` / `@actual-app/core` type definitions (not the docs, which disagree):

- **Import field name is `category`** (holds the Actual category *id*), parallel to the existing `payee` id field. `ImportTransactionEntity.category?: string` — *not* `category_id`. An unknown field would be silently ignored, so this name is load-bearing.
- **`getCategories()`** returns entries with `id`, `name`, `group_id`. Real categories have a `group_id`; category *groups* do not. The shim filters on `group_id` presence to return only assignable categories.
- **Graceful degradation:** if the user has **zero** Actual categories, the category dimension auto-settles to "uncategorised" everywhere and never prompts (degrades to Phase-1 behavior).

### The two-dimension hold/resolve model (the core mental model)

Every held `PendingReview` has two dimensions, each either **settled** or **unsettled**:

- **Payee dimension** is settled when `create_new == true` OR `resolve_to` is non-empty (today's behavior, unchanged).
- **Category dimension** is settled when `category_resolved == true`. A skip settles it with an empty `resolve_category_to` (= import uncategorised).

`take_resolved` imports an entry only when **both** dimensions are settled.

When a transaction is held (because at least one dimension is uncertain), each dimension is **pre-settled if it was confident**:

- Payee confident & exists → pre-settle: `resolve_to = payee_name` (no payee prompt will be sent).
- Payee uncertain → `suggestion` / `confidence` set (drives the payee prompt), `resolve_to` empty.
- Category confident & exists → pre-settle: `category_resolved = true`, `resolve_category_to = category_name` (no category prompt).
- Category uncertain → `category_suggestion` / `category_confidence` set (drives the category prompt), `category_resolved = false`.
- No categories exist at all → category dimension pre-settled as `category_resolved = true`, `resolve_category_to = ""` (uncategorised, no prompt).

The fully-confident-in-both case never becomes a held entry: it flows through the existing fast `matched` path (imported + cached immediately).

---

## File structure / responsibilities

- `actual-shim/index.js` — add `get_categories` op; `import_transactions` already forwards `req.transactions` verbatim, so the new `category` field flows once Rust serialises it.
- `src/actual/shim_protocol.rs` — `Category` struct; `ShimRequest::GetCategories`; `ImportTxn.category`.
- `src/actual/mod.rs` — `ActualClient::get_categories()`.
- `src/claude.rs` — rename `PayeeMatch` → `Classification` (adds `payee_confidence`, `category`, `category_confidence`); `ClaudeMatcher::classify`; updated system prompt + tool schema + `parse_response`.
- `src/state.rs` — `State.category_cache`; `PendingReview` category fields; `ResolvedReview.category_name`; category resolution methods; `take_resolved` two-dimension gating.
- `src/run.rs` — `ResolutionPlan`/`plan_resolution` (both caches); `MatchOutcome`/`HeldClassification` + `apply_matches` two-dimension gating; `run()` import-row category threading + fail-loud; `build_prompt` category flavor + prompt dispatch.
- `src/poll.rs` — sequential payee→category resolution.
- `src/telegram.rs` — category `ReplyAction` variants / callback action codes.
- `RUNBOOK.md` — document category classification + the no-create-new-category workaround.

---

## Task 1: Shim `get_categories` op + protocol `Category`, `GetCategories`, `ImportTxn.category`

**Files:**
- Modify: `actual-shim/index.js` (add a `case 'get_categories'` to `handle`)
- Modify: `src/actual/shim_protocol.rs`
- Test: `src/actual/shim_protocol.rs` (`mod tests`)

This task is additive and keeps the build green: existing `ImportTxn` construction sites set `category: None`, which serialises to nothing.

- [ ] **Step 1: Add the failing protocol tests**

In `src/actual/shim_protocol.rs`, inside `mod tests`, add:

```rust
    #[test]
    fn get_categories_request_serialises_and_category_deserialises() {
        let req = ShimRequest::GetCategories;
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            r#"{"op":"get_categories"}"#
        );

        let cat: Category =
            serde_json::from_str(r#"{"id":"cat-1","name":"Groceries"}"#).unwrap();
        assert_eq!(
            cat,
            Category {
                id: "cat-1".into(),
                name: "Groceries".into()
            }
        );
    }

    #[test]
    fn import_txn_omits_category_when_none_and_includes_when_some() {
        let none = ImportTxn {
            date: "2026-05-30".into(),
            amount: -450,
            payee: "p1".into(),
            category: None,
            imported_id: "tx1".into(),
        };
        let v = serde_json::to_value(&none).unwrap();
        assert!(v.get("category").is_none(), "category must be omitted when None");

        let some = ImportTxn {
            date: "2026-05-30".into(),
            amount: -450,
            payee: "p1".into(),
            category: Some("cat-1".into()),
            imported_id: "tx1".into(),
        };
        let v2 = serde_json::to_value(&some).unwrap();
        assert_eq!(v2["category"], "cat-1");
    }
```

Also update the existing `import_request_serialises_with_op_tag` test's `ImportTxn { … }` literal to include `category: None,` (after `payee`).

- [ ] **Step 2: Run the tests to verify they fail to compile**

Run: `cargo test -p actual-budget-automation shim_protocol 2>&1 | tee /tmp/t1.txt; grep -E "error\[|cannot find|no field" /tmp/t1.txt`
Expected: compile errors — `GetCategories`, `Category`, and the `category` field don't exist yet.

- [ ] **Step 3: Add `GetCategories`, `Category`, and `ImportTxn.category`**

In `src/actual/shim_protocol.rs`, add the variant to `ShimRequest` (after `GetScheduled`):

```rust
    GetCategories,
```

Add `category` to `ImportTxn` (after `payee`), with skip-if-none so the field is omitted when unset:

```rust
    /// Existing Actual payee **id** (never a name — avoids auto-create).
    pub payee: String,
    /// Existing Actual category **id**; omitted from the wire when `None`
    /// (Actual leaves the transaction uncategorised).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
```

Add the `Category` struct (next to `Payee`):

```rust
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Category {
    pub id: String,
    pub name: String,
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p actual-budget-automation shim_protocol 2>&1 | tee /tmp/t1.txt; grep -E "test result: ok|error\[" /tmp/t1.txt`
Expected: `test result: ok`. (The whole crate will not yet build if other `ImportTxn` sites lack `category`; fix any such site by adding `category: None,` — at this point only `src/run.rs`'s `make_row` closure and the resolved-row literal construct `ImportTxn`. Add `category: None,` to both so the crate compiles. They get real values in Task 7.)

Specifically in `src/run.rs`:
- In the `make_row` closure (~line 488), add `category: None,` after `payee: payee_id.to_string(),`.
- In the resolved-rows `ImportTxn { … }` literal (~line 615), add `category: None,` after `payee: pid,`.

- [ ] **Step 5: Add the shim `get_categories` op**

In `actual-shim/index.js`, inside `handle(req)`'s `switch`, add after the `get_payees` case:

```javascript
    case 'get_categories': {
      const cats = await api.getCategories();
      // Real categories carry a group_id; category *groups* do not — exclude
      // groups so only assignable categories are offered.
      return cats
        .filter((c) => c && c.group_id)
        .map((c) => ({ id: c.id, name: c.name }));
    }
```

- [ ] **Step 6: Verify the whole crate builds + fmt**

Run: `cargo build -p actual-budget-automation 2>&1 | tee /tmp/t1b.txt; grep -E "error\[|error:" /tmp/t1b.txt; cargo fmt --all`
Expected: no errors.

- [ ] **Step 7: Commit**

```bash
git add src/actual/shim_protocol.rs actual-shim/index.js src/run.rs
git commit -m "feat(actual): get_categories shim op + ImportTxn.category field"
```

---

## Task 2: `ActualClient::get_categories()`

**Files:**
- Modify: `src/actual/mod.rs`
- Test: `src/actual/mod.rs` (`mod tests`) — if the module has integration-style tests; otherwise rely on the run() integration tests. A focused unit test via a fake shim is added here.

- [ ] **Step 1: Write the failing test**

In `src/actual/mod.rs`'s `mod tests`, add a test that drives a tiny fake shim returning categories. Match the existing test style in that module (a small Node script written to a temp dir). If `mod tests` does not already exist in `src/actual/mod.rs`, add this minimal one:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn fake_shim(dir: &tempfile::TempDir, script: &str) -> ActualClient {
        std::fs::write(dir.path().join("index.js"), script).unwrap();
        ActualClient::new(
            dir.path().to_path_buf(),
            true,
            "http://t".into(),
            "pw".into(),
            "sync".into(),
        )
    }

    #[tokio::test]
    async fn get_categories_parses_id_and_name() {
        let dir = tempfile::tempdir().unwrap();
        let script = r"
let b='';process.stdin.on('data',c=>b+=c);process.stdin.on('end',()=>{
  const req=JSON.parse(b); const out=o=>process.stdout.write(JSON.stringify(o));
  if(req.op==='get_categories'){ out({ok:true,data:[{id:'c1',name:'Groceries'},{id:'c2',name:'Transport'}]}); }
  else { out({ok:false,error:'unknown op'}); }
});
";
        let client = fake_shim(&dir, script);
        let cats = client.get_categories().await.unwrap();
        assert_eq!(cats.len(), 2);
        assert_eq!(cats[0].name, "Groceries");
        assert_eq!(cats[1].id, "c2");
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p actual-budget-automation actual::tests::get_categories 2>&1 | tee /tmp/t2.txt; grep -E "error\[|no method|test result" /tmp/t2.txt`
Expected: compile error — `get_categories` not found.

- [ ] **Step 3: Implement `get_categories`**

In `src/actual/mod.rs`, add to `impl ActualClient` (next to `get_payees`, ~line 115), and add `Category` to the `use` of `shim_protocol`:

```rust
    /// # Errors
    /// Returns an error if the shim fails to spawn, exits non-zero, or returns invalid JSON.
    pub async fn get_categories(&self) -> anyhow::Result<Vec<shim_protocol::Category>> {
        self.call(&shim_protocol::ShimRequest::GetCategories).await
    }
```

(If the module imports specific items from `shim_protocol`, add `Category` and `GetCategories` is reached via `ShimRequest`. Use whatever import style the file already uses; the line above uses fully-qualified paths to avoid depending on existing `use`s.)

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p actual-budget-automation actual::tests::get_categories 2>&1 | tee /tmp/t2.txt; grep -E "test result: ok|error" /tmp/t2.txt`
Expected: `test result: ok`.

- [ ] **Step 5: Commit**

```bash
git add src/actual/mod.rs
git commit -m "feat(actual): ActualClient::get_categories"
```

---

## Task 3: `src/state.rs` — category cache, `PendingReview` category fields, `ResolvedReview.category_name`, two-dimension `take_resolved`, resolution methods

**Files:**
- Modify: `src/state.rs`
- Test: `src/state.rs` (`mod tests`)

This task is additive at the struct level (new `#[serde(default)]` fields) but changes `take_resolved` semantics. Existing `run.rs` construction sites are updated in later tasks; to keep the crate compiling at the end of this task, the new `PendingReview` fields are added with `#[serde(default)]` and the existing `add_pending` / `add_pending_full` set them to their "unsettled / not-applicable" defaults. Critically, to avoid regressing existing held entries (and the existing `run.rs` integration tests) before the category pipeline is wired, **`add_pending` and `add_pending_full` set `category_resolved = true`** (category not applicable) so a payee-only resolution still imports. The category pipeline (Task 7) uses a new constructor that sets `category_resolved` correctly.

- [ ] **Step 1: Write failing tests**

In `src/state.rs`'s `mod tests`, add:

```rust
    #[test]
    fn take_resolved_waits_for_both_dimensions() {
        let mut s = State::default();
        // payee resolved, category NOT resolved -> must stay pending
        s.pending_reviews.push(PendingReview {
            external_id: "tx1".into(),
            source_monzo_id: "acc_1".into(),
            raw_merchant: "TESCO".into(),
            amount: -500,
            date: "2026-06-01".into(),
            resolve_to: "Tesco".into(),
            create_new: false,
            suggestion: None,
            confidence: None,
            category_suggestion: Some("Groceries".into()),
            category_confidence: Some("medium".into()),
            resolve_category_to: String::new(),
            category_resolved: false,
            telegram_message_id: None,
        });
        let taken = s.take_resolved();
        assert!(taken.is_empty(), "payee-only resolution must not import yet");
        assert_eq!(s.pending_reviews.len(), 1);

        // now settle the category dimension via confirm
        assert!(s.confirm_category("tx1"));
        let taken2 = s.take_resolved();
        assert_eq!(taken2.len(), 1);
        assert_eq!(taken2[0].payee_name, "Tesco");
        assert_eq!(taken2[0].category_name.as_deref(), Some("Groceries"));
        // category folded into the category cache
        assert_eq!(s.category_cache.get("TESCO").map(String::as_str), Some("Groceries"));
    }

    #[test]
    fn skip_category_settles_as_uncategorised() {
        let mut s = State::default();
        s.pending_reviews.push(PendingReview {
            external_id: "tx2".into(),
            source_monzo_id: "acc_1".into(),
            raw_merchant: "ODD SHOP".into(),
            amount: -100,
            date: "2026-06-01".into(),
            resolve_to: "Odd Shop".into(),
            create_new: false,
            suggestion: None,
            confidence: None,
            category_suggestion: Some("Groceries".into()),
            category_confidence: Some("low".into()),
            resolve_category_to: String::new(),
            category_resolved: false,
            telegram_message_id: None,
        });
        assert!(s.skip_category("tx2"));
        let taken = s.take_resolved();
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].category_name, None, "skipped category imports uncategorised");
        // a skipped (empty) category is NOT written to the cache
        assert!(s.category_cache.get("ODD SHOP").is_none());
    }

    #[test]
    fn remap_category_sets_resolve_target() {
        let mut s = State::default();
        s.pending_reviews.push(PendingReview {
            external_id: "tx3".into(),
            source_monzo_id: "acc_1".into(),
            raw_merchant: "X".into(),
            amount: -10,
            date: "2026-06-01".into(),
            resolve_to: "X Payee".into(),
            create_new: false,
            suggestion: None,
            confidence: None,
            category_suggestion: Some("Groceries".into()),
            category_confidence: Some("low".into()),
            resolve_category_to: String::new(),
            category_resolved: false,
            telegram_message_id: None,
        });
        assert!(s.remap_category("tx3", "Dining Out"));
        assert!(s.pending_reviews[0].category_resolved);
        assert_eq!(s.pending_reviews[0].resolve_category_to, "Dining Out");
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p actual-budget-automation state:: 2>&1 | tee /tmp/t3.txt; grep -E "error\[|no field|no method|test result" /tmp/t3.txt`
Expected: compile errors — new fields/methods/`category_cache`/`ResolvedReview.category_name` missing.

- [ ] **Step 3: Add `category_cache` to `State`**

In `src/state.rs`, in `struct State` (after `payee_cache`):

```rust
    /// raw merchant -> resolved Actual category name.
    #[serde(default)]
    pub category_cache: BTreeMap<String, String>,
```

- [ ] **Step 4: Add category fields to `PendingReview`**

In `struct PendingReview` (after `confidence`, before `telegram_message_id`):

```rust
    /// Claude's proposed category for a medium/low-confidence held category.
    /// `None` = no category proposal (genuine no-match or not applicable).
    #[serde(default)]
    pub category_suggestion: Option<String>,
    /// "medium" | "low" when this was a confidence-held category proposal.
    #[serde(default)]
    pub category_confidence: Option<String>,
    /// Set to an existing category name to map to it. Empty = uncategorised.
    #[serde(default)]
    pub resolve_category_to: String,
    /// True once the category dimension is settled (confirmed / remapped /
    /// skipped / not-applicable). The txn imports only when this is true AND the
    /// payee dimension is settled.
    #[serde(default)]
    pub category_resolved: bool,
```

- [ ] **Step 5: Add `category_name` to `ResolvedReview`**

In `struct ResolvedReview` (after `payee_name` / `create_new`):

```rust
    /// Final category name to use, or `None` for uncategorised.
    pub category_name: Option<String>,
```

- [ ] **Step 6: Update `take_resolved` for two-dimension gating + category cache fold**

Replace the body of `take_resolved` (lines ~151-186) with:

```rust
    /// Remove fully-resolved pending reviews (both payee AND category dimensions
    /// settled), fold their raw->payee and raw->category mappings into the caches,
    /// and return them for import. Entries with either dimension unsettled remain.
    pub fn take_resolved(&mut self) -> Vec<ResolvedReview> {
        let mut resolved = Vec::new();
        let mut remaining = Vec::new();
        for pr in std::mem::take(&mut self.pending_reviews) {
            // Payee dimension (unchanged logic).
            let payee = if pr.create_new {
                let name = if !pr.resolve_to.is_empty() {
                    pr.resolve_to.clone()
                } else if let Some(s) = &pr.suggestion {
                    s.clone()
                } else {
                    pr.raw_merchant.clone()
                };
                Some((name, true))
            } else if !pr.resolve_to.is_empty() {
                Some((pr.resolve_to.clone(), false))
            } else {
                None
            };
            // Both dimensions must be settled to import.
            let Some((payee_name, create_new)) = payee else {
                remaining.push(pr);
                continue;
            };
            if !pr.category_resolved {
                remaining.push(pr);
                continue;
            }
            // Category name: empty resolve target = uncategorised (None).
            let category_name = if pr.resolve_category_to.is_empty() {
                None
            } else {
                Some(pr.resolve_category_to.clone())
            };
            self.payee_cache
                .insert(pr.raw_merchant.clone(), payee_name.clone());
            if let Some(cat) = &category_name {
                self.category_cache.insert(pr.raw_merchant.clone(), cat.clone());
            }
            resolved.push(ResolvedReview {
                external_id: pr.external_id,
                raw_merchant: pr.raw_merchant,
                amount: pr.amount,
                date: pr.date,
                payee_name,
                create_new,
                category_name,
                source_monzo_id: pr.source_monzo_id,
            });
        }
        self.pending_reviews = remaining;
        resolved
    }
```

- [ ] **Step 7: Default category fields in existing constructors**

In `add_pending` and `add_pending_full`, set the new `PendingReview` fields. To preserve today's payee-only behavior for these (used by paths not yet category-aware), set `category_resolved: true` and the rest empty/None:

For both constructors' `PendingReview { … }` literal, add before `telegram_message_id: None,`:

```rust
            category_suggestion: None,
            category_confidence: None,
            resolve_category_to: String::new(),
            category_resolved: true,
```

(Find `add_pending`'s literal too — it is built inside `add_pending` at ~lines 197-216; apply the same four lines.)

- [ ] **Step 8: Add category resolution methods**

In `impl State`, after `skip_pending` (~line 299), add:

```rust
    /// Confirm the held entry's suggested category (category_suggestion -> resolve target).
    pub fn confirm_category(&mut self, external_id: &str) -> bool {
        if let Some(pr) = self
            .pending_reviews
            .iter_mut()
            .find(|p| p.external_id == external_id)
        {
            pr.resolve_category_to = pr.category_suggestion.clone().unwrap_or_default();
            pr.category_resolved = true;
            true
        } else {
            false
        }
    }

    /// Remap the held entry's category to a user-supplied name.
    pub fn remap_category(&mut self, external_id: &str, name: &str) -> bool {
        if let Some(pr) = self
            .pending_reviews
            .iter_mut()
            .find(|p| p.external_id == external_id)
        {
            pr.resolve_category_to = name.to_string();
            pr.category_resolved = true;
            true
        } else {
            false
        }
    }

    /// Skip the category — import uncategorised (settles the dimension).
    pub fn skip_category(&mut self, external_id: &str) -> bool {
        if let Some(pr) = self
            .pending_reviews
            .iter_mut()
            .find(|p| p.external_id == external_id)
        {
            pr.resolve_category_to = String::new();
            pr.category_resolved = true;
            true
        } else {
            false
        }
    }
```

- [ ] **Step 9: Run tests + fix any other `ResolvedReview`/`PendingReview` literal sites**

Run: `cargo test -p actual-budget-automation state:: 2>&1 | tee /tmp/t3.txt; grep -E "test result: ok|error\[" /tmp/t3.txt`
Expected: `test result: ok`. If the crate fails to compile elsewhere because `ResolvedReview` is built without `category_name` or `PendingReview` literals in `src/run.rs` tests lack the new fields, add `category_name: None,` / the four category fields respectively. (The `run.rs` `build_prompt` tests construct `PendingReview` literals — add the four new fields with `category_resolved: true` there to compile; their behavior is asserted later.)

- [ ] **Step 10: Commit**

```bash
git add src/state.rs src/run.rs
git commit -m "feat(state): category cache + two-dimension pending review resolve"
```

---

## Task 4: `src/claude.rs` — `Classification` (rename from `PayeeMatch`), `classify`, category in schema/prompt/parse

**Files:**
- Modify: `src/claude.rs`
- Modify: `src/run.rs` (call site + `apply_matches` signature param type + `FakeClaude` test double)
- Test: `src/claude.rs` (`mod tests`)

This renames the type and trait method and threads categories through the request/response. It must be done together with the `run.rs` call sites so the crate compiles at commit time. `apply_matches`'s *gating logic* changes in Task 5; here we only update the type name it accepts and the call site, keeping payee-only behavior intact (category ignored for now by passing an empty category list and not yet gating on it).

- [ ] **Step 1: Update the claude tests to the new shape**

In `src/claude.rs`'s `mod tests`, replace `PayeeMatch` usages with `Classification` and add category assertions. Replace `parse_reads_confidence_field`, `parse_extracts_matches_with_nulls`, `request_has_cached_payee_block_and_tool_choice`, and `http_claude_posts_and_parses` with:

```rust
    #[test]
    fn parse_reads_both_dimensions() {
        let resp = serde_json::json!({
            "content": [
                { "type": "tool_use", "name": "report_matches", "input": {
                    "matches": [
                        { "raw": "TESCO", "payee": "Tesco", "payee_confidence": "high",
                          "category": "Groceries", "category_confidence": "high" },
                        { "raw": "SQ *CAFE", "payee": "Local Cafe", "payee_confidence": "low",
                          "category": null, "category_confidence": "low" }
                    ]
                }}
            ]
        });
        let out = parse_response(&resp).unwrap();
        assert_eq!(out[0].payee.as_deref(), Some("Tesco"));
        assert_eq!(out[0].payee_confidence, Confidence::High);
        assert_eq!(out[0].category.as_deref(), Some("Groceries"));
        assert_eq!(out[0].category_confidence, Confidence::High);
        assert_eq!(out[1].category, None);
        assert_eq!(out[1].category_confidence, Confidence::Low);
    }

    #[test]
    fn request_has_cached_payee_and_category_blocks_and_schema() {
        let req = build_request(
            "claude-sonnet-4-6",
            &["TFL TRAVEL CH".to_string()],
            &["Transport for London".to_string()],
            &["Transport".to_string(), "Groceries".to_string()],
        );
        let sys = req["system"].as_array().unwrap();
        // system blocks: instructions + payees + categories
        assert_eq!(sys.len(), 3);
        assert!(sys[1]["text"].as_str().unwrap().contains("Transport for London"));
        assert!(sys[2]["text"].as_str().unwrap().contains("Groceries"));
        assert_eq!(sys[2]["cache_control"]["type"], "ephemeral");
        let required =
            req["tools"][0]["input_schema"]["properties"]["matches"]["items"]["required"]
                .as_array()
                .unwrap();
        assert!(required.iter().any(|v| v == "category"));
        assert!(required.iter().any(|v| v == "category_confidence"));
        assert!(required.iter().any(|v| v == "payee_confidence"));
    }

    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn http_claude_posts_and_parses() {
        let server = MockServer::start().await;
        let body = serde_json::json!({
            "content": [{ "type": "tool_use", "name": "report_matches", "input": {
                "matches": [{ "raw": "TFL TRAVEL CH", "payee": "Transport for London",
                  "payee_confidence": "high", "category": "Transport", "category_confidence": "high" }]
            }}]
        });
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "test-key"))
            .and(header("anthropic-version", "2023-06-01"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let client = HttpClaude::new("test-key".into(), "claude-sonnet-4-6".into())
            .with_base_url(server.uri());
        let out = client
            .classify(
                &["TFL TRAVEL CH".into()],
                &["Transport for London".into()],
                &["Transport".into()],
            )
            .await
            .unwrap();
        assert_eq!(out[0].payee.as_deref(), Some("Transport for London"));
        assert_eq!(out[0].category.as_deref(), Some("Transport"));
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p actual-budget-automation claude:: 2>&1 | tee /tmp/t4.txt; grep -E "error\[|cannot find|test result" /tmp/t4.txt`
Expected: compile errors — `Classification`, new fields, new `classify` signature, 4-arg `build_request`.

- [ ] **Step 3: Replace `PayeeMatch` with `Classification` and update the trait**

In `src/claude.rs`, replace the `PayeeMatch` struct + `ClaudeMatcher` trait:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    pub raw: String,
    pub payee: Option<String>,
    pub payee_confidence: Confidence,
    pub category: Option<String>,
    pub category_confidence: Confidence,
}

#[async_trait]
pub trait ClaudeMatcher {
    /// For each raw merchant, return the matched existing payee and category
    /// (each `None` if no clear match), with a confidence per dimension.
    async fn classify(
        &self,
        raw: &[String],
        existing_payees: &[String],
        existing_categories: &[String],
    ) -> anyhow::Result<Vec<Classification>>;
}
```

- [ ] **Step 4: Update `SYSTEM_INSTRUCTIONS`**

```rust
const SYSTEM_INSTRUCTIONS: &str = "You normalise bank merchant strings to a user's existing budget payees and categories. \
For each raw merchant name, choose the single best-matching payee from the provided payee list (or null if none is a clear match), \
and the single best-matching category from the provided category list (or null if none is a clear match). \
Be conservative: prefer null over a wrong guess, for both payee and category. \
Rate your confidence in EACH choice independently as \"high\", \"medium\", or \"low\": \
\"high\" = an obvious, unambiguous match a human would not second-guess; \
\"medium\" = a plausible match with some doubt; \"low\" = a weak guess. \
Use \"low\" when the corresponding value is null. \
Call the report_matches tool with one entry per raw merchant, preserving the exact raw strings.";
```

- [ ] **Step 5: Update `build_request` to add the category system block + schema fields**

Replace `build_request` with:

```rust
/// Build the Anthropic Messages API request body. The existing-payee and
/// existing-category lists are separate cache-controlled system blocks so they
/// are billed cheaply on repeat runs.
pub fn build_request(
    model: &str,
    raw: &[String],
    existing_payees: &[String],
    existing_categories: &[String],
) -> Value {
    json!({
        "model": model,
        "max_tokens": 2048,
        "system": [
            { "type": "text", "text": SYSTEM_INSTRUCTIONS },
            {
                "type": "text",
                "text": format!("Existing payees (one per line):\n{}", existing_payees.join("\n")),
                "cache_control": { "type": "ephemeral" }
            },
            {
                "type": "text",
                "text": format!("Existing categories (one per line):\n{}", existing_categories.join("\n")),
                "cache_control": { "type": "ephemeral" }
            }
        ],
        "tools": [{
            "name": "report_matches",
            "description": "Report the chosen payee and category for each raw merchant name.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "matches": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "raw": { "type": "string" },
                                "payee": { "type": ["string", "null"] },
                                "payee_confidence": { "type": "string", "enum": ["high", "medium", "low"] },
                                "category": { "type": ["string", "null"] },
                                "category_confidence": { "type": "string", "enum": ["high", "medium", "low"] }
                            },
                            "required": ["raw", "payee", "payee_confidence", "category", "category_confidence"]
                        }
                    }
                },
                "required": ["matches"]
            }
        }],
        "tool_choice": { "type": "tool", "name": "report_matches" },
        "messages": [{
            "role": "user",
            "content": format!("Raw merchant names (one per line):\n{}", raw.join("\n"))
        }]
    })
}
```

- [ ] **Step 6: Update `parse_response`**

Replace `parse_response` with:

```rust
/// Extract `Classification`s from the Messages API response.
pub fn parse_response(resp: &Value) -> anyhow::Result<Vec<Classification>> {
    let content = resp["content"]
        .as_array()
        .context("response missing content array")?;
    let tool_use = content
        .iter()
        .find(|b| b["type"] == "tool_use" && b["name"] == "report_matches")
        .ok_or_else(|| anyhow!("no report_matches tool_use block in response"))?;
    let matches = tool_use["input"]["matches"]
        .as_array()
        .context("tool input missing matches array")?;
    let parse_opt = |v: &Value, field: &str| -> anyhow::Result<Option<String>> {
        match &v[field] {
            Value::Null => Ok(None),
            Value::String(s) => Ok(Some(s.clone())),
            other => Err(anyhow!("unexpected {field} value: {other}")),
        }
    };
    let mut out = Vec::with_capacity(matches.len());
    for m in matches {
        let raw = m["raw"].as_str().context("match missing raw")?.to_string();
        let payee = parse_opt(m, "payee")?;
        let category = parse_opt(m, "category")?;
        let payee_confidence = Confidence::parse(m["payee_confidence"].as_str().unwrap_or("medium"));
        let category_confidence =
            Confidence::parse(m["category_confidence"].as_str().unwrap_or("medium"));
        out.push(Classification {
            raw,
            payee,
            payee_confidence,
            category,
            category_confidence,
        });
    }
    Ok(out)
}
```

- [ ] **Step 7: Update the `HttpClaude` impl**

Replace the `impl ClaudeMatcher for HttpClaude` body's method:

```rust
#[async_trait]
impl ClaudeMatcher for HttpClaude {
    async fn classify(
        &self,
        raw: &[String],
        existing_payees: &[String],
        existing_categories: &[String],
    ) -> anyhow::Result<Vec<Classification>> {
        if raw.is_empty() {
            return Ok(Vec::new());
        }
        let req = build_request(&self.model, raw, existing_payees, existing_categories);
        let resp = self
            .http
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&req)
            .send()
            .await
            .context("anthropic request failed")?;
        let status = resp.status();
        let value: Value = resp.json().await.context("anthropic response not JSON")?;
        if !status.is_success() {
            return Err(anyhow!("anthropic error {status}: {value}"));
        }
        parse_response(&value)
    }
}
```

- [ ] **Step 8: Update `src/run.rs` imports + `FakeClaude` + call site (compile only; gating unchanged)**

In `src/run.rs`:
- Change `use crate::claude::{ClaudeMatcher, Confidence, PayeeMatch};` → `use crate::claude::{ClaudeMatcher, Classification, Confidence};`.
- In `mod tests`, change `use crate::claude::{ClaudeMatcher, Confidence, PayeeMatch};` → `... Classification ...` and update `FakeClaude`:

```rust
    struct FakeClaude {
        out: Vec<Classification>,
    }
    #[async_trait]
    impl ClaudeMatcher for FakeClaude {
        async fn classify(
            &self,
            _raw: &[String],
            _p: &[String],
            _c: &[String],
        ) -> anyhow::Result<Vec<Classification>> {
            Ok(self.out.clone())
        }
    }
```

- In `run()` (~lines 462-471), update the call. For now pass an empty categories slice and keep `apply_matches` payee-only; categories are wired in Task 5. Temporarily build the matches as `Vec<Classification>`:

```rust
    let category_names: Vec<String> = Vec::new(); // wired in Task 5
    let claude_matches: Vec<Classification> = if plan.needs_claude.is_empty() {
        Vec::new()
    } else {
        claude
            .classify(&plan.needs_claude, &payee_names, &category_names)
            .await
            .context("claude classify")?
    };
```

- `apply_matches` currently takes `matches: &[PayeeMatch]` and reads `m.payee` / `m.confidence`. Update its parameter type to `&[Classification]` and, **for this task only**, read `m.payee_confidence` in place of `m.confidence` so payee-only behavior is preserved and the crate compiles. (Full two-dimension gating replaces this in Task 5.) Update the existing `apply_matches_partitions_by_confidence` test's `PayeeMatch { … confidence: … }` literals to `Classification { raw, payee, payee_confidence: …, category: None, category_confidence: Confidence::Low }`.

- The two integration tests that build `FakeClaude { out: vec![PayeeMatch { … }] }` must become `Classification { … }` with `payee_confidence`, `category: None`, `category_confidence: Confidence::Low`. At this task they still **pass** with payee-only behavior — `run()` does not yet call `get_categories` and `MatchOutcome` is unchanged — so this is the minimal compile fix. They are fully adapted to the two-dimension world in Task 5 (Step 9b), once `run()` starts requiring categories.

- [ ] **Step 9: Run the full test suite**

Run: `cargo test -p actual-budget-automation 2>&1 | tee /tmp/t4.txt; grep -E "test result:|error\[|FAILED" /tmp/t4.txt`
Expected: all green. Investigate any `FAILED`/`error[` before continuing.

- [ ] **Step 10: clippy + fmt**

Run: `cargo clippy --all-targets -- -D warnings 2>&1 | tee /tmp/t4c.txt; grep -E "warning:|error" /tmp/t4c.txt; cargo fmt --all`
Expected: no warnings/errors.

- [ ] **Step 11: Commit**

```bash
git add src/claude.rs src/run.rs
git commit -m "feat(claude): combined payee+category Classification call"
```

---

## Task 5: `apply_matches` two-dimension gating + `MatchOutcome`/`HeldClassification`

**Files:**
- Modify: `src/run.rs`
- Test: `src/run.rs` (`mod tests`)

- [ ] **Step 1: Write the failing gating tests**

Replace `apply_matches_partitions_by_confidence` with a matrix test:

```rust
    fn cls(raw: &str, payee: Option<&str>, pc: Confidence, cat: Option<&str>, cc: Confidence) -> Classification {
        Classification {
            raw: raw.into(),
            payee: payee.map(Into::into),
            payee_confidence: pc,
            category: cat.map(Into::into),
            category_confidence: cc,
        }
    }

    #[test]
    fn apply_matches_gates_on_both_dimensions() {
        let txs = vec![
            tx("1", "TESCO", -500),    // both high + exist -> matched
            tx("2", "CAFE", -450),     // payee high+exist, category low -> held, payee pre-settled
            tx("3", "MYSTERY", -100),  // payee none -> held, payee unsettled
        ];
        let matches = vec![
            cls("TESCO", Some("Tesco"), Confidence::High, Some("Groceries"), Confidence::High),
            cls("CAFE", Some("Local Cafe"), Confidence::High, Some("Dining"), Confidence::Low),
            cls("MYSTERY", None, Confidence::Low, None, Confidence::Low),
        ];
        let payees: BTreeSet<&str> = ["Tesco", "Local Cafe"].into_iter().collect();
        let cats: BTreeSet<&str> = ["Groceries", "Dining"].into_iter().collect();
        let outcome = apply_matches(&txs, &matches, Confidence::High, &payees, &cats, true);

        assert_eq!(outcome.matched, vec![("1".to_string(), "Tesco".to_string(), "Groceries".to_string())]);
        assert_eq!(outcome.held.len(), 2);

        let h2 = outcome.held.iter().find(|h| h.tx.external_id == "2").unwrap();
        assert_eq!(h2.payee_resolved.as_deref(), Some("Local Cafe")); // pre-settled
        assert_eq!(h2.category_suggestion.as_deref(), Some("Dining"));
        assert_eq!(h2.category_resolved, None); // uncertain -> prompt

        let h3 = outcome.held.iter().find(|h| h.tx.external_id == "3").unwrap();
        assert_eq!(h3.payee_resolved, None);
        assert_eq!(h3.payee_suggestion, None);
    }

    #[test]
    fn apply_matches_autosettles_category_when_none_exist() {
        let txs = vec![tx("1", "TESCO", -500)];
        let matches = vec![cls("TESCO", Some("Tesco"), Confidence::High, None, Confidence::Low)];
        let payees: BTreeSet<&str> = ["Tesco"].into_iter().collect();
        let cats: BTreeSet<&str> = BTreeSet::new();
        // categories_available = false -> category dimension not applicable -> matched, category None
        let outcome = apply_matches(&txs, &matches, Confidence::High, &payees, &cats, false);
        assert_eq!(outcome.matched.len(), 1);
        assert_eq!(outcome.matched[0].2, String::new(), "uncategorised sentinel");
    }
```

Note: `matched` carries `(external_id, payee_name, category_name)`. When categories are unavailable, `category_name` is the empty string (uncategorised). When available and both confident, it is the confident category name.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p actual-budget-automation run::tests::apply_matches 2>&1 | tee /tmp/t5.txt; grep -E "error\[|test result" /tmp/t5.txt`
Expected: compile errors (signature, `MatchOutcome.held`, `HeldClassification`).

- [ ] **Step 3: Replace `MatchOutcome` + add `HeldClassification`**

```rust
/// One held transaction with per-dimension state for review.
#[derive(Debug, PartialEq, Eq)]
pub struct HeldClassification {
    pub tx: RawTransaction,
    /// `Some(name)` if the payee was confident & exists (pre-settled, no prompt).
    pub payee_resolved: Option<String>,
    /// Suggestion to prompt for when the payee is uncertain.
    pub payee_suggestion: Option<String>,
    pub payee_confidence: Option<Confidence>,
    /// `Some(name)` if the category was confident & exists (pre-settled, no prompt).
    pub category_resolved: Option<String>,
    /// Suggestion to prompt for when the category is uncertain.
    pub category_suggestion: Option<String>,
    pub category_confidence: Option<Confidence>,
}

/// Result of applying Claude classifications, split by the auto-import cutoff.
#[derive(Debug, PartialEq, Eq)]
pub struct MatchOutcome {
    /// (`external_id`, `payee_name`, `category_name`) confident in BOTH dimensions
    /// (category_name empty = uncategorised when no categories exist). Imported now.
    pub matched: Vec<(String, String, String)>,
    /// Held: at least one dimension uncertain.
    pub held: Vec<HeldClassification>,
}
```

- [ ] **Step 4: Rewrite `apply_matches`**

```rust
// `matches` (input) and `matched` (auto-imported output) are distinct concepts
// despite their lexical similarity.
#[allow(clippy::similar_names)]
pub fn apply_matches(
    txs: &[RawTransaction],
    matches: &[Classification],
    autoimport: Confidence,
    existing_payees: &BTreeSet<&str>,
    existing_categories: &BTreeSet<&str>,
    categories_available: bool,
) -> MatchOutcome {
    let lookup: BTreeMap<&str, &Classification> =
        matches.iter().map(|m| (m.raw.as_str(), m)).collect();
    let mut matched = Vec::new();
    let mut held = Vec::new();
    for tx in txs {
        let Some(m) = lookup.get(tx.raw_merchant.as_str()) else {
            // No classification at all -> hold, both dimensions unsettled
            // (category auto-settled to uncategorised if none exist).
            held.push(HeldClassification {
                tx: tx.clone(),
                payee_resolved: None,
                payee_suggestion: None,
                payee_confidence: None,
                category_resolved: if categories_available { None } else { Some(String::new()) },
                category_suggestion: None,
                category_confidence: None,
            });
            continue;
        };

        // Payee dimension.
        let payee_ok = m
            .payee
            .as_deref()
            .is_some_and(|p| m.payee_confidence.rank() >= autoimport.rank() && existing_payees.contains(p));

        // Category dimension. Not applicable (no categories) counts as settled-uncategorised.
        let category_state: CatState = if !categories_available {
            CatState::Settled(String::new())
        } else if let Some(cat) = m.category.as_deref() {
            if m.category_confidence.rank() >= autoimport.rank() && existing_categories.contains(cat) {
                CatState::Settled(cat.to_string())
            } else {
                CatState::Uncertain
            }
        } else {
            CatState::Uncertain
        };

        match (payee_ok, &category_state) {
            (true, CatState::Settled(cat)) => {
                // Both confident -> import now.
                matched.push((tx.external_id.clone(), m.payee.clone().unwrap(), cat.clone()));
            }
            _ => {
                // Hold; pre-settle whichever dimension is confident.
                let payee_resolved = payee_ok.then(|| m.payee.clone().unwrap());
                let (payee_suggestion, payee_confidence) = if payee_ok {
                    (None, None)
                } else {
                    (m.payee.clone(), m.payee.as_ref().map(|_| m.payee_confidence))
                };
                let (category_resolved, category_suggestion, category_confidence) =
                    match &category_state {
                        CatState::Settled(cat) => (Some(cat.clone()), None, None),
                        CatState::Uncertain => (
                            None,
                            m.category.clone(),
                            m.category.as_ref().map(|_| m.category_confidence),
                        ),
                    };
                held.push(HeldClassification {
                    tx: tx.clone(),
                    payee_resolved,
                    payee_suggestion,
                    payee_confidence,
                    category_resolved,
                    category_suggestion,
                    category_confidence,
                });
            }
        }
    }
    MatchOutcome { matched, held }
}

enum CatState {
    Settled(String),
    Uncertain,
}
```

- [ ] **Step 5: Update the `run()` call site of `apply_matches`**

In `run()` replace the existing `apply_matches` call and surrounding category-list setup (Task 4 left `category_names` empty). Add, after `get_payees`/`get_scheduled` (and before building `payee_names`), the category fetch:

```rust
    let categories = actual.get_categories().await.context("get_categories")?;
    let category_names: Vec<String> = categories.iter().map(|c| c.name.clone()).collect();
    let categories_available = !category_names.is_empty();
```

Add `use crate::actual::shim_protocol::Category;` if needed (or qualify). Then change the classify call to pass `&category_names`, and the gating call:

```rust
    let existing_payees: BTreeSet<&str> = payee_names.iter().map(String::as_str).collect();
    let existing_cats: BTreeSet<&str> = category_names.iter().map(String::as_str).collect();
    let outcome = apply_matches(
        &unknown_txs,
        &claude_matches,
        autoimport,
        &existing_payees,
        &existing_cats,
        categories_available,
    );
```

(The previous `let existing: BTreeSet<&str> = …` line is replaced by `existing_payees`.)

The `run()` body still references `outcome.matched` (now 3-tuples) and `outcome.proposed`/`outcome.unmatched` (now gone), so the crate will not compile until the import-assembly section is rewritten. That rewrite is **part of this same task** (Steps 6–9 below); do not commit until the crate compiles and the full suite is green at Step 11.

- [ ] **Step 6: Rewrite `run()` import-assembly to thread category ids**

Add a category name→id resolver near `payee_id_for`:

```rust
/// Map a category name to its id. Returns `None` (→ caller holds, fail-loud) if
/// the name is unknown OR ambiguous: Actual permits same-named categories in
/// different groups, and we must not silently pick the wrong one.
fn category_id_for<'a>(cats: &'a [Category], name: &str) -> Option<&'a str> {
    let mut matches = cats.iter().filter(|c| c.name == name);
    let first = matches.next()?;
    if matches.next().is_some() {
        warn!("category name '{name}' is ambiguous (>1 group); holding for review");
        return None;
    }
    Some(first.id.as_str())
}
```

Add a focused unit test for this helper in `mod tests`:

```rust
    #[test]
    fn category_id_for_holds_on_ambiguous_name() {
        use crate::actual::shim_protocol::Category;
        let cats = vec![
            Category { id: "c1".into(), name: "Misc".into() },
            Category { id: "c2".into(), name: "Misc".into() },
            Category { id: "c3".into(), name: "Groceries".into() },
        ];
        assert_eq!(category_id_for(&cats, "Groceries"), Some("c3"));
        assert_eq!(category_id_for(&cats, "Misc"), None, "duplicate -> None (hold)");
        assert_eq!(category_id_for(&cats, "Nope"), None);
    }
```

Update `make_row` to accept an optional category id:

```rust
    let make_row = |tx: &RawTransaction, payee_id: &str, category_id: Option<String>| ImportTxn {
        date: tx.date.format("%Y-%m-%d").to_string(),
        amount: tx.amount_minor,
        payee: payee_id.to_string(),
        category: category_id,
        imported_id: tx.external_id.clone(),
    };
```

**Cached hits** (`plan.cached` now yields `(external_id, payee_name, category_name)`):

```rust
    for (ext_id, payee_name, category_name) in &plan.cached {
        let cat_id = if category_name.is_empty() {
            Some(None) // uncategorised, valid
        } else {
            category_id_for(&categories, category_name).map(|id| Some(id.to_string()))
        };
        match (by_id.get(ext_id.as_str()), payee_id_for(&payees, payee_name), cat_id) {
            (Some(tx), Some(pid), Some(cat)) => {
                if let Some(aid) = actual_id_for(&tx.source_monzo_id, &account_links) {
                    grouped.entry(aid).or_default().push(make_row(tx, pid, cat));
                } else {
                    // unmapped account -> hold (unchanged helper)
                    hold_unmapped(state, tx, &mut held);
                }
            }
            (Some(tx), None, _) => {
                warn!("cached payee '{payee_name}' for tx {ext_id} no longer exists; holding");
                hold_cached_missing(state, tx, payee_name, &mut held);
            }
            (Some(tx), Some(_), None) => {
                warn!("cached category '{category_name}' for tx {ext_id} no longer exists; holding");
                hold_cached_missing(state, tx, payee_name, &mut held);
            }
            _ => {}
        }
    }
```

To keep this readable, add two small helpers above `run()` (or inline if preferred). Because they need `state`, define them as closures or free functions taking `&mut State`:

```rust
fn hold_unmapped(state: &mut State, tx: &RawTransaction, held: &mut usize) {
    let date = tx.date.format("%Y-%m-%d").to_string();
    if state.add_pending(&tx.external_id, &tx.raw_merchant, tx.amount_minor, &date, &tx.source_monzo_id) {
        *held += 1;
    }
}

fn hold_cached_missing(state: &mut State, tx: &RawTransaction, payee_name: &str, held: &mut usize) {
    let date = tx.date.format("%Y-%m-%d").to_string();
    if state.add_pending_full(
        &tx.external_id, &tx.raw_merchant, tx.amount_minor, &date,
        Some(payee_name.to_string()), None, &tx.source_monzo_id,
    ) {
        *held += 1;
    }
}
```

**Matched (Claude, both-confident)** — now 3-tuples with category name (possibly empty):

```rust
    let mut newly_cached: Vec<(String, String)> = Vec::new();
    for (ext_id, payee_name, category_name) in &outcome.matched {
        let cat_id = if category_name.is_empty() {
            Some(None)
        } else {
            category_id_for(&categories, category_name).map(|id| Some(id.to_string()))
        };
        match (by_id.get(ext_id.as_str()), payee_id_for(&payees, payee_name), cat_id) {
            (Some(tx), Some(pid), Some(cat)) => {
                if let Some(aid) = actual_id_for(&tx.source_monzo_id, &account_links) {
                    grouped.entry(aid).or_default().push(make_row(tx, pid, cat));
                    if !state.payee_cache.contains_key(&tx.raw_merchant) {
                        state.payee_cache.insert(tx.raw_merchant.clone(), payee_name.clone());
                        newly_cached.push((tx.raw_merchant.clone(), payee_name.clone()));
                    }
                    if !category_name.is_empty() {
                        state.category_cache.insert(tx.raw_merchant.clone(), category_name.clone());
                    }
                } else {
                    hold_unmapped(state, tx, &mut held);
                }
            }
            (Some(tx), None, _) | (Some(tx), Some(_), None) => {
                warn!("matched payee/category for tx {ext_id} unexpectedly missing; holding");
                hold_cached_missing(state, tx, payee_name, &mut held);
            }
            _ => {}
        }
    }
```

**Resolved pending reviews** — now carry `category_name: Option<String>`:

```rust
    for r in &resolved {
        let pid = match payee_id_for(&payees, &r.payee_name) {
            Some(pid) => pid.to_string(),
            None if r.create_new && !dry_run => actual.create_payee(r.payee_name.clone()).await?.id,
            None => {
                warn!("payee '{}' not found and not creating (dry_run or create_new=false)", r.payee_name);
                continue;
            }
        };
        // Resolve the category name to an id; empty/None = uncategorised.
        let cat_id: Option<String> = match &r.category_name {
            None => None,
            Some(name) => match category_id_for(&categories, name) {
                Some(id) => Some(id.to_string()),
                None => {
                    // Fail-loud: resolved category no longer exists -> re-hold.
                    warn!("resolved category '{name}' for '{}' no longer exists; re-holding", r.raw_merchant);
                    if state.add_pending_full(
                        &r.external_id, &r.raw_merchant, r.amount, &r.date,
                        Some(r.payee_name.clone()), None, &r.source_monzo_id,
                    ) {
                        held += 1;
                    }
                    continue;
                }
            },
        };
        if let Some(aid) = actual_id_for(&r.source_monzo_id, &account_links) {
            grouped.entry(aid).or_default().push(ImportTxn {
                date: r.date.clone(),
                amount: r.amount,
                payee: pid,
                category: cat_id,
                imported_id: if r.external_id.is_empty() {
                    format!("{}:{}", r.raw_merchant, r.date)
                } else {
                    r.external_id.clone()
                },
            });
        } else {
            warn!("resolved review '{}' from unmapped account '{}' — re-holding", r.raw_merchant, r.source_monzo_id);
            if state.add_pending_full(
                &r.external_id, &r.raw_merchant, r.amount, &r.date,
                Some(r.payee_name.clone()), None, &r.source_monzo_id,
            ) {
                held += 1;
            }
        }
    }
```

**Held classifications** — replace the old `outcome.proposed` + `outcome.unmatched` loops with a single loop that adds a pending review carrying both dimensions. Add a new `State` constructor `add_pending_classified` (see Step 7) and call it:

```rust
    for h in &outcome.held {
        let date = h.tx.date.format("%Y-%m-%d").to_string();
        if state.add_pending_classified(&h.tx, &date, h) {
            held += 1;
        }
    }
```

- [ ] **Step 7: Add `add_pending_classified` to `State`**

In `src/state.rs` `impl State`, add (it consumes a `&HeldClassification`; to avoid a `run`→`state` type dependency, accept the already-decomposed parts instead):

```rust
    /// Add a held review carrying both payee and category dimension state.
    /// `payee_resolved`/`category_resolved` pre-settle a confident dimension so
    /// no prompt is sent for it.
    #[allow(clippy::too_many_arguments)]
    pub fn add_pending_classified(
        &mut self,
        external_id: &str,
        raw_merchant: &str,
        amount: i64,
        date: &str,
        source_monzo_id: &str,
        payee_resolved: Option<String>,
        payee_suggestion: Option<String>,
        payee_confidence: Option<String>,
        category_resolved: Option<String>,
        category_suggestion: Option<String>,
        category_confidence: Option<String>,
    ) -> bool {
        if self.pending_reviews.iter().any(|p| p.external_id == external_id) {
            return false;
        }
        let (resolve_to, create_new) = (payee_resolved.unwrap_or_default(), false);
        let (resolve_category_to, cat_resolved) = match category_resolved {
            Some(c) => (c, true),
            None => (String::new(), false),
        };
        self.pending_reviews.push(PendingReview {
            external_id: external_id.to_string(),
            source_monzo_id: source_monzo_id.to_string(),
            raw_merchant: raw_merchant.to_string(),
            amount,
            date: date.to_string(),
            resolve_to,
            create_new,
            suggestion: payee_suggestion,
            confidence: payee_confidence,
            category_suggestion,
            category_confidence,
            resolve_category_to,
            category_resolved: cat_resolved,
            telegram_message_id: None,
        });
        true
    }
```

Then in `run()`'s held loop, call it with the decomposed `HeldClassification` fields (convert `Confidence` to `String` via `as_str().to_string()`):

```rust
    for h in &outcome.held {
        let date = h.tx.date.format("%Y-%m-%d").to_string();
        let added = state.add_pending_classified(
            &h.tx.external_id,
            &h.tx.raw_merchant,
            h.tx.amount_minor,
            &date,
            &h.tx.source_monzo_id,
            h.payee_resolved.clone(),
            h.payee_suggestion.clone(),
            h.payee_confidence.map(|c| c.as_str().to_string()),
            h.category_resolved.clone(),
            h.category_suggestion.clone(),
            h.category_confidence.map(|c| c.as_str().to_string()),
        );
        if added {
            held += 1;
        }
    }
```

- [ ] **Step 8: Update the dry-run reporting block**

Replace the `outcome.proposed` / `outcome.unmatched` dry-run print loops with one held loop:

```rust
        for h in &outcome.held {
            let p = h.payee_resolved.as_deref().or(h.payee_suggestion.as_deref()).unwrap_or("?");
            let c = h.category_resolved.as_deref().or(h.category_suggestion.as_deref()).unwrap_or("?");
            println!("    ? hold {} {} (payee {p}, category {c})", h.tx.raw_merchant, h.tx.amount_minor);
        }
```

- [ ] **Step 9: Update `plan_resolution` / `ResolutionPlan` for both caches**

```rust
#[derive(Debug, PartialEq, Eq)]
pub struct ResolutionPlan {
    /// (`external_id`, `payee_name`, `category_name`) for FULL cache hits.
    /// `category_name` empty = cached as uncategorised.
    pub cached: Vec<(String, String, String)>,
    pub needs_claude: Vec<String>,
}

pub fn plan_resolution(
    txs: &[RawTransaction],
    payee_cache: &BTreeMap<String, String>,
    category_cache: &BTreeMap<String, String>,
) -> ResolutionPlan {
    let mut cached = Vec::new();
    let mut needs_claude = Vec::new();
    for tx in txs {
        match (payee_cache.get(&tx.raw_merchant), category_cache.get(&tx.raw_merchant)) {
            (Some(payee), Some(category)) => {
                cached.push((tx.external_id.clone(), payee.clone(), category.clone()));
            }
            // Payee cached but category not (legacy entry) -> needs Claude.
            _ if !needs_claude.contains(&tx.raw_merchant) => needs_claude.push(tx.raw_merchant.clone()),
            _ => {}
        }
    }
    ResolutionPlan { cached, needs_claude }
}
```

Update the `run()` call: `let plan = plan_resolution(&candidates, &state.payee_cache, &state.category_cache);`

Update the `splits_cache_hits_unknowns_for_claude` test to seed both caches and expect a 3-tuple. Replace it with:

```rust
    #[test]
    fn plan_needs_claude_unless_both_caches_hit() {
        let mut payee = BTreeMap::new();
        let mut cat = BTreeMap::new();
        payee.insert("TFL TRAVEL CH".to_string(), "Transport for London".to_string());
        cat.insert("TFL TRAVEL CH".to_string(), "Transport".to_string());
        // AMZN only in payee cache -> still needs claude (no category)
        payee.insert("AMZNMKTPLACE".to_string(), "Amazon".to_string());
        let txs = vec![tx("1", "TFL TRAVEL CH", -275), tx("2", "AMZNMKTPLACE", -1999)];
        let plan = plan_resolution(&txs, &payee, &cat);
        assert_eq!(plan.cached, vec![("1".into(), "Transport for London".to_string(), "Transport".to_string())]);
        assert_eq!(plan.needs_claude, vec!["AMZNMKTPLACE".to_string()]);
    }
```

- [ ] **Step 9a: Teach the fake shim scripts the `get_categories` op**

`run()` now calls `get_categories`, so every shim-script test double a `run()` test exercises must answer it or those tests fail with `unknown op`. In `mod tests`, add this branch to the `fake_actual` and `fake_actual_routing` script strings (and any other script a `run()` test uses), giving two categories:

```javascript
  else if(req.op==='get_categories'){ out({ok:true,data:[{id:'c_groc',name:'Groceries'},{id:'c_din',name:'Dining'}]}); }
```

Do **not** change `fake_actual_failing_get_payees` — `run()` calls `get_payees` before `get_categories`, so that test still fails at the same (pre-category) point. Keep the `get_payees`-before-`get_categories` order in `run()`.

- [ ] **Step 9b: Adapt the two existing integration tests to the two-dimension world**

`run()` now holds a transaction unless the category dimension is also settled, so the two pre-existing `run()` integration tests need confident, existing categories to behave as before:

- `imports_route_to_per_source_actual_account` (uses High-confidence payees that exist): set each `Classification`'s `category: Some("Groceries")` (a category the shim returns) and `category_confidence: Confidence::High`, so both dimensions are confident → still auto-imports, now with a category. Its routing assertions are unaffected.
- `medium_match_held_prompted_then_imported_after_confirm` (tests the payee held→confirm→import flow): keep the payee Medium (held, payee uncertain) but make the **category confident & existing** — `category: Some("Dining")`, `category_confidence: Confidence::High` — so the category dimension is pre-settled and only the payee is reviewed. After `state.confirm_pending("tx_cafe")` settles the payee, both dimensions are settled and Run #2 imports exactly as the test already asserts. (The held entry's category was pre-settled, so no category prompt is sent and the existing `channel.sent` count of 1 still holds.)

- [ ] **Step 10: Run full suite + clippy + fmt**

Run: `cargo test -p actual-budget-automation 2>&1 | tee /tmp/t5.txt; grep -E "test result:|error\[|FAILED" /tmp/t5.txt`
Then: `cargo clippy --all-targets -- -D warnings 2>&1 | tee /tmp/t5c.txt; grep -E "warning:|error" /tmp/t5c.txt; cargo fmt --all`
Expected: all green, no clippy warnings. With Steps 9a–9b done, the two existing integration tests pass under the new model.

- [ ] **Step 11: Commit**

```bash
git add src/run.rs src/state.rs
git commit -m "feat(run): gate auto-import on both payee and category; thread category id"
```

---

## Task 6: `build_prompt` category flavor + `run()` prompt dispatch (payee then category)

**Files:**
- Modify: `src/run.rs` (`build_prompt`, the run() prompt-dispatch block)
- Modify: `src/telegram.rs` (callback action codes for category, if encoded distinctly)
- Test: `src/run.rs` (`mod tests`)

The dispatch rule: for each held entry that has not been prompted yet (`telegram_message_id.is_none()`), send a prompt for the **first unsettled dimension** — payee if the payee dimension is unsettled, else category if the category dimension is unsettled. (An entry with both settled is not held; an entry with payee settled + category unsettled prompts category.)

- [ ] **Step 1: Write failing prompt tests**

Add to `mod tests`:

```rust
    #[test]
    fn build_prompt_category_flavor_when_payee_settled() {
        let payees = vec!["Local Cafe".to_string()];
        let cats = vec!["Dining".to_string(), "Groceries".to_string()];
        let entry = PendingReview {
            external_id: "tx1".into(),
            source_monzo_id: String::new(),
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
        let p = build_prompt(&entry, &payees, &cats);
        assert!(p.chat_text.to_lowercase().contains("category"), "category prompt: {}", p.chat_text);
        assert!(p.chat_text.contains("'Dining'"));
        // confirm-category + skip-category buttons
        assert_eq!(p.buttons.len(), 2);
        assert_eq!(p.buttons[0].1, "cc:tx1");
        assert_eq!(p.buttons[1].1, "xc:tx1");
    }

    #[test]
    fn build_prompt_payee_first_when_payee_unsettled() {
        let payees = vec!["Local Cafe".to_string()];
        let cats = vec!["Dining".to_string()];
        let entry = PendingReview {
            external_id: "tx2".into(),
            source_monzo_id: String::new(),
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
        let p = build_prompt(&entry, &payees, &cats);
        // payee prompt first (unchanged flavor): confirm/create/skip
        assert_eq!(p.buttons.len(), 3);
        assert_eq!(p.buttons[0].1, "cf:tx2");
    }
```

Update the existing `build_prompt_*` tests' calls to pass a third `&[]` categories argument, and add the four new category fields to their `PendingReview` literals (with `category_resolved: true` so they take the payee flavor).

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p actual-budget-automation run::tests::build_prompt 2>&1 | tee /tmp/t6.txt; grep -E "error\[|test result" /tmp/t6.txt`
Expected: compile errors (arity, action codes).

- [ ] **Step 3: Add category action codes to telegram callback parsing**

In `src/telegram.rs`, find where callback action codes (`cf`, `nw`, `sk`) are decoded into `ReplyAction`. Add two new codes and map them to category actions. Add to `enum ReplyAction`:

```rust
    /// Confirm the suggested category.
    ConfirmCategory,
    /// Skip the category (import uncategorised).
    SkipCategory,
```

In the callback decode match (where `"cf" => ReplyAction::Confirm`, etc.), add:

```rust
        "cc" => ReplyAction::ConfirmCategory,
        "xc" => ReplyAction::SkipCategory,
```

A free-text reply remains `ReplyAction::Remap(text)` and is interpreted by `poll` against the entry's current dimension (Task 7). (No change needed to `encode_callback`.)

- [ ] **Step 4: Rewrite `build_prompt` to choose dimension**

```rust
fn build_prompt(entry: &PendingReview, payee_names: &[String], category_names: &[String]) -> Prompt {
    use crate::telegram::encode_callback;
    let key = &entry.external_id;
    let buttons = |specs: &[(&str, &str)]| -> Vec<(String, String)> {
        specs
            .iter()
            .map(|(label, action)| encode_callback(action, key).map(|d| (label.to_string(), d)))
            .collect::<Option<Vec<_>>>()
            .unwrap_or_default()
    };

    // Payee dimension settled when create_new or resolve_to set.
    let payee_settled = entry.create_new || !entry.resolve_to.is_empty();
    if !payee_settled {
        // --- payee prompt (unchanged flavors) ---
        return match &entry.suggestion {
            Some(s) if payee_names.iter().any(|p| p == s) => {
                let conf = entry.confidence.as_deref().unwrap_or("medium");
                Prompt {
                    chat_text: format!(
                        "'{}' -> '{}'?  ({} confidence)\nReply with a different payee to remap.",
                        entry.raw_merchant, s, conf
                    ),
                    buttons: buttons(&[("✅ Confirm", "cf"), ("🆕 Create new", "nw"), ("❌ Skip", "sk")]),
                }
            }
            Some(s) => Prompt {
                chat_text: format!("No existing payee '{s}'. Create it?\nOr reply with a payee name."),
                buttons: buttons(&[("🆕 Create", "nw"), ("❌ Skip", "sk")]),
            },
            None => Prompt {
                chat_text: format!(
                    "New merchant '{}' — which payee?\nReply with a payee name, or:",
                    entry.raw_merchant
                ),
                buttons: buttons(&[("🆕 Create new", "nw"), ("❌ Skip", "sk")]),
            },
        };
    }

    // --- category prompt (payee already settled) ---
    match &entry.category_suggestion {
        Some(s) if category_names.iter().any(|c| c == s) => {
            let conf = entry.category_confidence.as_deref().unwrap_or("medium");
            Prompt {
                chat_text: format!(
                    "Category for '{}' -> '{}'?  ({} confidence)\nReply with a different category, or Skip to leave it uncategorised.",
                    entry.raw_merchant, s, conf
                ),
                buttons: buttons(&[("✅ Confirm", "cc"), ("❌ Skip", "xc")]),
            }
        }
        _ => Prompt {
            chat_text: format!(
                "Which category for '{}'?\nReply with a category name, or Skip to leave it uncategorised.",
                entry.raw_merchant
            ),
            buttons: buttons(&[("❌ Skip", "xc")]),
        },
    }
}
```

- [ ] **Step 5: Update the `run()` prompt-dispatch call**

In `run()` (the `if let Some(ch) = channel` block ~line 738), pass `category_names` to `build_prompt`:

```rust
            let prompt = build_prompt(&state.pending_reviews[i], &payee_names, &category_names);
```

- [ ] **Step 6: Run tests + clippy + fmt**

Run: `cargo test -p actual-budget-automation run::tests::build_prompt 2>&1 | tee /tmp/t6.txt; grep -E "test result: ok|error\[|FAILED" /tmp/t6.txt`
Then full suite + `cargo clippy --all-targets -- -D warnings` + `cargo fmt --all`.
Expected: green.

- [ ] **Step 7: Commit**

```bash
git add src/run.rs src/telegram.rs
git commit -m "feat(telegram): sequential payee-then-category review prompt"
```

---

## Task 7: `src/poll.rs` — apply category replies; re-prompt remaining dimension

**Files:**
- Modify: `src/poll.rs`
- Test: `src/poll.rs` (`mod tests`)

`poll` maps each held entry's `telegram_message_id` to its `external_id`. When a reply arrives, the action is applied to the entry. A `Remap(text)` free-text reply must target the **currently-prompted dimension**: payee if the payee dimension is unsettled, else category. Confirm/CreateNew/Skip remain payee actions; the new ConfirmCategory/SkipCategory are category actions. After applying, the entry's `telegram_message_id` is cleared if a further dimension still needs prompting, so `run()` re-prompts it next cycle. (Clearing on dimension transition: when a payee reply settles the payee dimension but the category dimension is still unsettled, clear `telegram_message_id` so the category prompt is sent next run.)

- [ ] **Step 1: Write the failing poll test**

```rust
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
            vec![Reply { entry_key: "tx1".into(), action: ReplyAction::ConfirmCategory }],
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
            vec![Reply { entry_key: "tx2".into(), action: ReplyAction::Remap("Groceries".into()) }],
            60,
        );
        poll(&cfg, &mut state, &channel).await.unwrap();
        assert!(state.pending_reviews[0].category_resolved);
        assert_eq!(state.pending_reviews[0].resolve_category_to, "Groceries");
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p actual-budget-automation poll:: 2>&1 | tee /tmp/t7.txt; grep -E "error\[|no variant|test result" /tmp/t7.txt`
Expected: compile errors (new `ReplyAction` variants not handled) and/or assertion failures.

- [ ] **Step 3: Update the `poll` apply loop**

Replace the `match &r.action { … }` block:

```rust
        // A free-text remap targets the currently-prompted dimension: payee if
        // the payee dimension is still unsettled, else category.
        let payee_settled = state
            .pending_reviews
            .iter()
            .find(|p| p.external_id == r.entry_key)
            .map(|p| p.create_new || !p.resolve_to.is_empty())
            .unwrap_or(false);
        let ok = match &r.action {
            ReplyAction::Confirm => state.confirm_pending(&r.entry_key),
            ReplyAction::CreateNew => state.create_pending(&r.entry_key),
            ReplyAction::Skip => state.skip_pending(&r.entry_key),
            ReplyAction::ConfirmCategory => state.confirm_category(&r.entry_key),
            ReplyAction::SkipCategory => state.skip_category(&r.entry_key),
            ReplyAction::Remap(text) => {
                if payee_settled {
                    state.remap_category(&r.entry_key, text)
                } else {
                    state.remap_pending(&r.entry_key, text)
                }
            }
        };
        if ok {
            // If a payee reply just settled the payee dimension but the category
            // dimension is still open, clear the prompt id so run() re-prompts
            // (now for the category). All-settled entries import next run.
            if let Some(pr) = state.pending_reviews.iter_mut().find(|p| p.external_id == r.entry_key) {
                let payee_now_settled = pr.create_new || !pr.resolve_to.is_empty();
                if payee_now_settled && !pr.category_resolved {
                    pr.telegram_message_id = None;
                }
            }
            applied += 1;
        }
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p actual-budget-automation poll:: 2>&1 | tee /tmp/t7.txt; grep -E "test result: ok|error\[|FAILED" /tmp/t7.txt`
Expected: `test result: ok`.

- [ ] **Step 5: Full suite + clippy + fmt**

Run: `cargo test -p actual-budget-automation 2>&1 | tee /tmp/t7all.txt; grep -E "test result:|FAILED|error\[" /tmp/t7all.txt`
Then `cargo clippy --all-targets -- -D warnings` + `cargo fmt --all`.
Expected: green.

- [ ] **Step 6: Commit**

```bash
git add src/poll.rs
git commit -m "feat(poll): apply category replies; re-prompt remaining dimension"
```

---

## Task 8: End-to-end integration tests

**Files:**
- Modify: `src/run.rs` (`mod tests`) — add two end-to-end integration tests (the shim doubles already answer `get_categories` from Task 5).

The shim-script doubles already answer `get_categories` (added in Task 5, Step 9a) returning `Groceries`/`Dining`. This task only adds two new end-to-end tests.

- [ ] **Step 1: Confirm the shim doubles return categories**

Verify `fake_actual` (used below) has the `get_categories` branch from Task 5 Step 9a (`c_groc`/`Groceries`, `c_din`/`Dining`). If any double a new test needs is missing it, add the branch now.

- [ ] **Step 2: Write failing integration test — both-confident auto-import sets category**

```rust
    #[tokio::test]
    async fn high_confidence_imports_with_category() {
        let shim = tempfile::tempdir().unwrap();
        let statedir = tempfile::tempdir().unwrap();
        let state_path = statedir.path().join("s.toml");
        let imports_log = shim.path().join("imports.log");
        let cfg = cfg_at(&state_path);
        let actual = fake_actual(&shim); // payees: Local Cafe; cats: Groceries, Dining
        let notifier = NullNotifier;
        let channel = FakeChannel::new(vec![], 0);

        let mut state = State::default();
        state.monzo.accounts.push(crate::state::AccountLink {
            monzo_id: "acc_1".into(),
            actual_id: "acc".into(),
            ..Default::default()
        });
        let mut bank = FakeBank { txs: vec![tx("tx_cafe", "SQ *CAFE", -450)] };
        let claude = FakeClaude {
            out: vec![Classification {
                raw: "SQ *CAFE".into(),
                payee: Some("Local Cafe".into()),
                payee_confidence: Confidence::High,
                category: Some("Dining".into()),
                category_confidence: Confidence::High,
            }],
        };
        run(&cfg, false, &mut state, &mut bank, &actual, &claude, &notifier,
            Some(&channel as &dyn ConfirmChannel), Confidence::High).await.unwrap();

        let logged = std::fs::read_to_string(&imports_log).unwrap();
        assert!(logged.contains("p_cafe"), "imported under Local Cafe id");
        assert!(logged.contains("c_din"), "imported with the Dining category id");
        assert!(state.pending_reviews.is_empty(), "nothing held");
        assert_eq!(state.category_cache.get("SQ *CAFE").map(String::as_str), Some("Dining"));
    }
```

(`fake_actual`'s `import_transactions` logs `req.transactions` JSON, which now includes `category`; asserting `c_din` confirms the id reached the shim.)

- [ ] **Step 3: Write failing integration test — confident payee, uncertain category → held, category prompt, then import after confirm**

```rust
    #[tokio::test]
    async fn uncertain_category_holds_then_imports_after_category_confirm() {
        let shim = tempfile::tempdir().unwrap();
        let statedir = tempfile::tempdir().unwrap();
        let state_path = statedir.path().join("s.toml");
        let imports_log = shim.path().join("imports.log");
        let cfg = cfg_at(&state_path);
        let actual = fake_actual(&shim);
        let notifier = NullNotifier;
        let channel = FakeChannel::new(vec![], 0);

        let mut state = State::default();
        state.monzo.accounts.push(crate::state::AccountLink {
            monzo_id: "acc_1".into(),
            actual_id: "acc".into(),
            ..Default::default()
        });
        let mut bank = FakeBank { txs: vec![tx("tx_cafe", "SQ *CAFE", -450)] };
        let claude = FakeClaude {
            out: vec![Classification {
                raw: "SQ *CAFE".into(),
                payee: Some("Local Cafe".into()),
                payee_confidence: Confidence::High, // payee confident + exists
                category: Some("Dining".into()),
                category_confidence: Confidence::Low, // category uncertain -> held
            }],
        };
        // Run #1: held (payee pre-settled), category prompt sent, nothing imported.
        run(&cfg, false, &mut state, &mut bank, &actual, &claude, &notifier,
            Some(&channel as &dyn ConfirmChannel), Confidence::High).await.unwrap();
        assert!(!imports_log.exists(), "must not import while category unsettled");
        assert_eq!(state.pending_reviews.len(), 1);
        assert!(state.pending_reviews[0].category_suggestion.as_deref() == Some("Dining"));
        let sent = channel.sent.lock().unwrap();
        assert!(sent[0].chat_text.to_lowercase().contains("category"), "category prompt sent");
        drop(sent);

        // User confirms the category (what poll would do).
        assert!(state.confirm_category("tx_cafe"));

        // Run #2: no new txs -> the held tx imports with the Dining category.
        let mut bank2 = FakeBank { txs: vec![] };
        let claude2 = FakeClaude { out: vec![] };
        run(&cfg, false, &mut state, &mut bank2, &actual, &claude2, &notifier,
            Some(&channel as &dyn ConfirmChannel), Confidence::High).await.unwrap();
        let logged = std::fs::read_to_string(&imports_log).unwrap();
        assert!(logged.contains("tx_cafe") && logged.contains("c_din"));
        assert!(state.pending_reviews.is_empty());
    }
```

- [ ] **Step 4: Run to verify failure, then ensure pass**

Run: `cargo test -p actual-budget-automation run::tests:: 2>&1 | tee /tmp/t8.txt; grep -E "test result:|FAILED|error\[" /tmp/t8.txt`
Expected initially: the two new tests fail/compile-error; iterate on the production code only if a genuine gap is found (the prior tasks should make them pass once the shim scripts answer `get_categories`).

- [ ] **Step 5: Full suite + clippy + fmt**

Run: `cargo test -p actual-budget-automation 2>&1 | tee /tmp/t8all.txt; grep -E "test result:|FAILED|error\[" /tmp/t8all.txt`
Then `cargo clippy --all-targets -- -D warnings` + `cargo fmt --all`.
Expected: all green.

- [ ] **Step 6: Commit**

```bash
git add src/run.rs
git commit -m "test(run): end-to-end category auto-import + held-category review"
```

---

## Task 9: Documentation (`RUNBOOK.md`)

**Files:**
- Modify: `RUNBOOK.md`

- [ ] **Step 1: Document category classification**

Add a "Categories (Phase 2)" section to `RUNBOOK.md` covering:

```markdown
## Categories (Phase 2)

`aba run` asks Claude to classify each new merchant into one of your **existing
Actual categories**, in the same call that picks the payee. A transaction is
imported automatically only when BOTH the payee and the category are confident
matches to things that exist in Actual. If either is uncertain, the whole
transaction is **held** and you are asked via Telegram — first for the payee (if
that is the uncertain part), then for the category.

- **Confirm** accepts the suggested category. **Skip** leaves the transaction
  uncategorised (it still imports). Reply with a different category name to remap.
- Categories are cached per merchant (like payees), so each merchant is normally
  only asked once.
- **No "create new category" via Telegram.** Actual categories must belong to a
  category group, so create new categories in the Actual UI, then reply with the
  new category's name (or let the next run re-suggest it).
- If you have **no categories** in Actual at all, categorisation is skipped
  entirely and transactions import uncategorised (Phase-1 behavior).
```

- [ ] **Step 2: Commit**

```bash
git add RUNBOOK.md
git commit -m "docs(runbook): document Phase 2 category classification"
```

---

## Final verification

- [ ] **Run the full gate**

Run:
```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```
Expected: fmt clean, no clippy warnings, all tests pass. (`cargo test`, not `--lib`, per project convention — runs unit + integration tests.)

- [ ] **Manual shim smoke test (operational, optional but recommended)**

On a machine that can reach Actual, confirm the live `@actual-app/api` honors the `category` field by importing one transaction with a known category id and reading it back in the Actual UI. This guards against any version-specific field-name drift (the installed type says `category`, but verify against the running server).
