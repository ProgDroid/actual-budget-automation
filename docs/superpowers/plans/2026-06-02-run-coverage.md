# `run()` Integration Coverage Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add integration tests for `run()`'s three highest-risk uncovered paths (AuthExpired early-return, dry-run state-splice, create_payee→import failure), plus the test doubles they need. No production code changes.

**Architecture:** All work lives in `src/run.rs` `mod tests`, reusing the Task 10 patterns (`cfg_at`, `fake_actual`, `tempdir`, `FakeBank`/`FakeClaude`). Each new test double is introduced in the same task as the test(s) that use it, so there is no `dead_code` window between commits.

**Tech Stack:** Rust 2024 (binary crate `aba`), `tokio::test`, `async_trait`, a Node fake shim, `tempfile`.

---

## File Structure

- **Modify** `src/run.rs` — `mod tests` only: 4 new doubles + 4 new tests.
- **Modify** `docs/PHASE2-BACKLOG.md` — correct the #4 AuthExpired bullet; mark #4 done.

No production code changes. Branch `feat/phase2-run-coverage`.

## Reference: existing test infrastructure (already in `src/run.rs` `mod tests`)

- `mod tests` starts with `use super::*;` plus `BankError, BankSource, RawTransaction`, `ClaudeMatcher, Confidence, PayeeMatch`, `Notifier`, `PendingReview`, `ConfirmChannel`, `async_trait`, `chrono::{DateTime, NaiveDate, Utc}`, `BTreeMap`, `Mutex`, `TempDir`.
- `tx(id, merchant, amount) -> RawTransaction` helper.
- `FakeBank { txs }`, `FakeClaude { out }`, `NullNotifier`, `FakeChannel`.
- `fake_actual(dir) -> ActualClient` (shim: get_payees → `[{p_cafe, Local Cafe}]`, get_scheduled → `[]`, create_payee → `{id:p_new}`, import_transactions → appends to `imports.log`, returns added count).
- `cfg_at(state_path) -> Config`.
- `run` signature: `run(&cfg, dry_run: bool, &mut state, &mut bank, &actual, &claude, &notifier, channel: Option<&dyn ConfirmChannel>, autoimport: Confidence)`.
- `State` derives `Default` + `PartialEq` (so `payee_cache`, `pending_reviews`, `last_run` compare directly); `State::save(&Path)` / `State::load(&Path)`.

## Reference: project harness gotchas

- Binary crate — `cargo test` / `cargo test <name>`, NEVER `cargo test --lib`.
- Treat rust-analyzer `<new-diagnostics>` as advisory/stale; trust `cargo build` / `cargo clippy --all-targets -- -D warnings` / `cargo test` and read the ACTUAL output.
- `cargo test` does not use `-D warnings`, so a transient `dead_code` warning never fails it; the final clippy gate (last task) is where warnings must be zero. This plan avoids dead_code windows anyway by pairing doubles with tests.

---

## Task 1: AuthExpired early-return tests (+ `RecordingNotifier`, `AuthExpiredBank`)

**Files:** `src/run.rs` (`mod tests`)

- [ ] **Step 1: Add the two doubles**

Add inside `mod tests` (e.g. after `NullNotifier`):

```rust
/// Notifier double that records every notify(title, body) call.
struct RecordingNotifier {
    calls: Mutex<Vec<(String, String)>>,
}
impl RecordingNotifier {
    fn new() -> Self {
        Self { calls: Mutex::new(Vec::new()) }
    }
}
#[async_trait]
impl Notifier for RecordingNotifier {
    async fn notify(&self, t: &str, b: &str) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push((t.to_string(), b.to_string()));
        Ok(())
    }
}

/// Bank whose `ensure_auth` always reports the 90-day re-auth condition.
struct AuthExpiredBank;
#[async_trait]
impl BankSource for AuthExpiredBank {
    fn name(&self) -> &'static str {
        "fake"
    }
    async fn ensure_auth(&mut self, _s: &mut State) -> Result<(), BankError> {
        Err(BankError::AuthExpired("test".into()))
    }
    async fn fetch_since(
        &self,
        _since: DateTime<Utc>,
    ) -> Result<Vec<RawTransaction>, BankError> {
        Ok(vec![]) // never reached
    }
}
```

- [ ] **Step 2: Write the two tests**

```rust
#[tokio::test]
async fn auth_expired_notifies_and_saves_when_not_dry_run() {
    let shim = tempfile::tempdir().unwrap();
    let statedir = tempfile::tempdir().unwrap();
    let state_path = statedir.path().join("s.toml");
    let imports_log = shim.path().join("imports.log");
    let cfg = cfg_at(&state_path);
    let actual = fake_actual(&shim);
    let notifier = RecordingNotifier::new();

    let mut state = State::default();
    let mut bank = AuthExpiredBank;
    let claude = FakeClaude { out: vec![] };
    run(
        &cfg, false, &mut state, &mut bank, &actual, &claude, &notifier, None, Confidence::High,
    )
    .await
    .unwrap();

    let calls = notifier.calls.lock().unwrap();
    assert_eq!(calls.len(), 1, "one re-auth notification");
    assert_eq!(calls[0].0, "Monzo re-auth required");
    assert!(state_path.exists(), "state saved on !dry_run auth-expired");
    assert!(!imports_log.exists(), "no fetch/import after auth-expired");
}

#[tokio::test]
async fn auth_expired_dry_run_does_not_notify_or_save() {
    let shim = tempfile::tempdir().unwrap();
    let statedir = tempfile::tempdir().unwrap();
    let state_path = statedir.path().join("s.toml");
    let cfg = cfg_at(&state_path);
    let actual = fake_actual(&shim);
    let notifier = RecordingNotifier::new();

    let mut state = State::default();
    let mut bank = AuthExpiredBank;
    let claude = FakeClaude { out: vec![] };
    run(
        &cfg, true, &mut state, &mut bank, &actual, &claude, &notifier, None, Confidence::High,
    )
    .await
    .unwrap();

    assert!(notifier.calls.lock().unwrap().is_empty(), "dry-run: no notify");
    assert!(!state_path.exists(), "dry-run auth-expired writes no state");
}
```

- [ ] **Step 3: Run the tests**

Run: `cargo test auth_expired`
Expected: PASS (2 tests). (Behavior already exists in `run.rs:176-189`; these are coverage tests, expected green immediately.)

- [ ] **Step 4: Commit**

```bash
git add src/run.rs
git commit -m "test(run): cover AuthExpired early-return (notify+save gated on !dry_run)"
```

---

## Task 2: Dry-run state-splice test (+ `RotatingBank`)

**Files:** `src/run.rs` (`mod tests`)

- [ ] **Step 1: Add the double**

```rust
/// Bank that simulates Monzo's refresh-token rotation during `ensure_auth`.
struct RotatingBank {
    rotate_to: String,
}
#[async_trait]
impl BankSource for RotatingBank {
    fn name(&self) -> &'static str {
        "fake"
    }
    async fn ensure_auth(&mut self, s: &mut State) -> Result<(), BankError> {
        s.monzo.refresh_token = self.rotate_to.clone();
        Ok(())
    }
    async fn fetch_since(
        &self,
        _since: DateTime<Utc>,
    ) -> Result<Vec<RawTransaction>, BankError> {
        Ok(vec![])
    }
}
```

- [ ] **Step 2: Write the test**

```rust
#[tokio::test]
async fn dry_run_persists_only_rotated_token() {
    use chrono::TimeZone;
    let shim = tempfile::tempdir().unwrap();
    let statedir = tempfile::tempdir().unwrap();
    let state_path = statedir.path().join("s.toml");
    let cfg = cfg_at(&state_path);
    let actual = fake_actual(&shim);
    let notifier = NullNotifier;

    // On-disk baseline: cache + a pending review + last_run + old token.
    // Built as a struct literal (not default-then-reassign) to avoid the
    // clippy::field_reassign_with_default lint, which is in the CI gate.
    let baseline = State {
        last_run: Some(Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap()),
        monzo: crate::state::MonzoState {
            refresh_token: "old".into(),
            account_id: "acc".into(),
        },
        payee_cache: BTreeMap::from([("RAW".to_string(), "Payee".to_string())]),
        pending_reviews: vec![PendingReview {
            external_id: "p1".into(),
            raw_merchant: "RAW".into(),
            amount: -100,
            date: "2026-06-01".into(),
            resolve_to: String::new(),
            create_new: false,
            suggestion: None,
            confidence: None,
            telegram_message_id: None,
        }],
        ..State::default()
    };
    baseline.save(&state_path).unwrap();

    let mut state = State::load(&state_path).unwrap();
    let mut bank = RotatingBank { rotate_to: "new".into() };
    let claude = FakeClaude { out: vec![] };
    run(
        &cfg, true, &mut state, &mut bank, &actual, &claude, &notifier, None, Confidence::High,
    )
    .await
    .unwrap();

    let on_disk = State::load(&state_path).unwrap();
    assert_eq!(on_disk.payee_cache, baseline.payee_cache, "cache unchanged");
    assert_eq!(on_disk.pending_reviews, baseline.pending_reviews, "pending unchanged");
    assert_eq!(on_disk.last_run, baseline.last_run, "last_run not advanced");
    assert_eq!(on_disk.monzo.refresh_token, "new", "rotated token persisted");
}
```

- [ ] **Step 3: Run the test**

Run: `cargo test dry_run_persists_only_rotated_token`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add src/run.rs
git commit -m "test(run): dry-run persists only the rotated Monzo token"
```

---

## Task 3: create_payee→import failure characterization (+ failing-import shim)

**Files:** `src/run.rs` (`mod tests`)

- [ ] **Step 1: Add the failing-import shim helper**

```rust
/// Like `fake_actual` but `import_transactions` fails, and `create_payee`
/// records that it was called (to `payees.log`) before returning an id.
fn fake_actual_failing_import(dir: &TempDir) -> ActualClient {
    let script = r#"
let b='';process.stdin.on('data',c=>b+=c);process.stdin.on('end',()=>{
  const req=JSON.parse(b); const out=o=>process.stdout.write(JSON.stringify(o));
  const fs=require('fs'), path=require('path');
  if(req.op==='get_payees'){ out({ok:true,data:[{id:'p_cafe',name:'Local Cafe'}]}); }
  else if(req.op==='get_scheduled'){ out({ok:true,data:[]}); }
  else if(req.op==='create_payee'){
    fs.appendFileSync(path.join(__dirname,'payees.log'), 'called\n');
    out({ok:true,data:{id:'p_new'}});
  }
  else if(req.op==='import_transactions'){ out({ok:false,error:'boom'}); }
  else { out({ok:false,error:'unknown op'}); }
});
"#;
    std::fs::write(dir.path().join("index.js"), script).unwrap();
    ActualClient::new(
        dir.path().to_path_buf(),
        false,
        "http://t".into(),
        "pw".into(),
        "sync".into(),
    )
}
```

- [ ] **Step 2: Write the test**

```rust
#[tokio::test]
async fn create_payee_import_failure_leaves_review_for_retry() {
    let shim = tempfile::tempdir().unwrap();
    let statedir = tempfile::tempdir().unwrap();
    let state_path = statedir.path().join("s.toml");
    let payees_log = shim.path().join("payees.log");
    let cfg = cfg_at(&state_path);
    let actual = fake_actual_failing_import(&shim);
    let notifier = NullNotifier;

    // A resolved create_new review (payee name falls back to raw_merchant).
    // Struct literal to avoid clippy::field_reassign_with_default.
    let mut state = State {
        monzo: crate::state::MonzoState {
            refresh_token: String::new(),
            account_id: "acc".into(),
        },
        pending_reviews: vec![PendingReview {
            external_id: "tx_x".into(),
            raw_merchant: "MyStore".into(),
            amount: -500,
            date: "2026-05-30".into(),
            resolve_to: String::new(),
            create_new: true,
            suggestion: None,
            confidence: None,
            telegram_message_id: None,
        }],
        ..State::default()
    };
    state.save(&state_path).unwrap(); // baseline on disk

    let mut bank = FakeBank { txs: vec![] };
    let claude = FakeClaude { out: vec![] };
    let result = run(
        &cfg, false, &mut state, &mut bank, &actual, &claude, &notifier, None, Confidence::High,
    )
    .await;

    assert!(result.is_err(), "import failure surfaces as Err (loud, not silent)");
    assert!(payees_log.exists(), "create_payee was attempted (transient orphan)");
    let on_disk = State::load(&state_path).unwrap();
    assert_eq!(
        on_disk.pending_reviews.len(),
        1,
        "resolved review still on disk -> retry is clean (state not saved on failure)"
    );
    assert_eq!(on_disk.pending_reviews[0].external_id, "tx_x");
}
```

- [ ] **Step 3: Run the test**

Run: `cargo test create_payee_import_failure`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add src/run.rs
git commit -m "test(run): characterize create_payee->import failure as recoverable"
```

---

## Task 4: Correct the backlog #4 note

**Files:** `docs/PHASE2-BACKLOG.md`

- [ ] **Step 1: Fix the AuthExpired bullet**

In `docs/PHASE2-BACKLOG.md` section 4, replace the bullet:

```markdown
- **`AuthExpired` early-return**: notification fires only when `!dry_run`; `save`
  is always attempted.
```

with:

```markdown
- **`AuthExpired` early-return**: notification **and** `save` both fire only when
  `!dry_run`; the run then early-returns `Ok` without fetching or importing.
```

- [ ] **Step 2: Commit**

```bash
git add docs/PHASE2-BACKLOG.md
git commit -m "docs: correct backlog #4 AuthExpired save note"
```

---

## Task 5: Full verification gate

**Files:** none (verification only).

- [ ] **Step 1: Clippy gate**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: zero warnings/errors. All four doubles are used by tests (no `dead_code`).

- [ ] **Step 2: Full test suite**

Run: `cargo test`
Expected: all pass — 68 pre-existing + 4 new = 72.

- [ ] **Step 3: Build sanity**

Run: `cargo build`
Expected: clean.

- [ ] **Step 4: Final commit (only if verification required a fix)**

```bash
git add -A
git commit -m "chore: cleanup for run() coverage tests"
```

---

## Self-Review

**Spec coverage:**
- AuthExpired notify+save gated on `!dry_run`, early return, no import → Task 1 (both dry and non-dry tests). ✓
- Dry-run splice: cache/pending/last_run unchanged, only rotated token persisted → Task 2. ✓
- create_payee→import failure: `Err`, review survives on disk, create attempted → Task 3. ✓
- New doubles `RecordingNotifier`, `AuthExpiredBank`, `RotatingBank`, `fake_actual_failing_import` → Tasks 1-3 (paired with their tests, no dead_code window). ✓
- Backlog correction → Task 4. ✓

**Placeholder scan:** No TBD/TODO; every test and double is shown in full.

**Type consistency:** `run(...)` arg order matches existing call sites; `PendingReview` constructed with all 9 fields (no `Default`); `State` fields (`payee_cache`, `pending_reviews`, `last_run`, `monzo.refresh_token`, `monzo.account_id`) match `src/state.rs`; `Notifier::notify(&self, &str, &str)`, `BankSource::{name, ensure_auth, fetch_since}`, `ClaudeMatcher`, `Confidence::High` all match existing trait defs and doubles. `use chrono::TimeZone;` added locally in Task 2's test for `Utc.with_ymd_and_hms`. ✓
