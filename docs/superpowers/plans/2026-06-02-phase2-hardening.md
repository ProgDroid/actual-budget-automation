# Phase 2 Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Three independent Phase 1-review hardening fixes: OAuth `state` CSRF verification, variable-amount schedule skip correctness, and a truthful dry-run "would hold" count.

**Architecture:** No architectural change. Item 1 adds a random nonce + a pure `verify_state` helper in `auth_monzo`. Item 2 makes the schedule amount `Option<i64>` end-to-end (shim → `ScheduleRow` → `Schedule`) and matches variable schedules on date+payee only. Item 3 factors the dry-run hold count into a pure helper that excludes already-pending entries.

**Tech Stack:** Rust 2024 (binary crate `aba`), `getrandom` (new direct dep, already in tree), `serde`, `chrono`, `strsim`; Node.js shim (`actual-shim/index.js`).

---

## File Structure

- **Modify** `Cargo.toml` — add `getrandom = "0.3"`.
- **Modify** `src/run.rs` — `auth_monzo` (nonce + verify), new `verify_state`/`random_state` helpers; dry-run block + new `would_hold_count` helper.
- **Modify** `src/matching.rs` — `Schedule.amount: Option<i64>`, `matches_scheduled` variable branch, tests.
- **Modify** `src/actual/shim_protocol.rs` — `ScheduleRow.amount: Option<i64>`, test.
- **Modify** `actual-shim/index.js` — emit `null` for non-numeric `_amount`.

Each task = one commit. Single branch `feat/phase2-hardening`.

## Reference: project harness gotchas

- `aba` is a **binary** crate — run `cargo test` / `cargo test <name>`, NEVER `cargo test --lib`.
- Treat any rust-analyzer `<new-diagnostics>` as advisory/stale; trust `cargo build` / `cargo clippy --all-targets -- -D warnings` / `cargo test` and read the ACTUAL output (grep for `error[`, `FAILED`).
- Clippy gate is default lints only; `unused_imports`/`dead_code` are default-on (a helper added but not yet wired can trip `dead_code` transiently between commits — it clears when wired in the same task).

---

## Task 1: OAuth `state` (CSRF) verification

**Files:**
- Modify: `Cargo.toml` (dependencies)
- Modify: `src/run.rs` (`auth_monzo` ~lines 452-515; add two helpers near it)
- Test: `src/run.rs` (`mod tests`)

- [ ] **Step 1: Add the dependency**

In `Cargo.toml` under `[dependencies]`, add:

```toml
getrandom = "0.3"
```

- [ ] **Step 2: Write the failing test for `verify_state`**

Add to `src/run.rs` `mod tests`:

```rust
#[test]
fn verify_state_accepts_exact_match() {
    assert!(super::verify_state("abc123", Some("abc123")).is_ok());
}

#[test]
fn verify_state_rejects_mismatch() {
    assert!(super::verify_state("abc123", Some("nope")).is_err());
}

#[test]
fn verify_state_rejects_missing() {
    assert!(super::verify_state("abc123", None).is_err());
}

#[test]
fn random_state_is_32_hex_chars_and_varies() {
    let a = super::random_state().unwrap();
    let b = super::random_state().unwrap();
    assert_eq!(a.len(), 32, "128 bits => 32 hex chars");
    assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    assert_ne!(a, b, "two nonces must differ");
}
```

- [ ] **Step 3: Run to verify it fails**

Run: `cargo test verify_state random_state`
Expected: FAIL to compile — `verify_state` / `random_state` not defined.

- [ ] **Step 4: Add the helpers**

Add these two free functions in `src/run.rs` immediately above `pub async fn auth_monzo` (around line 452):

```rust
/// Generate an unguessable 128-bit OAuth `state` nonce, hex-encoded (32 chars).
fn random_state() -> anyhow::Result<String> {
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf).map_err(|e| anyhow::anyhow!("RNG failure generating state: {e}"))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// Verify the OAuth `state` echoed back on the redirect matches what we sent.
/// # Errors
/// Errors if `returned` is absent or does not equal `expected` (possible CSRF).
fn verify_state(expected: &str, returned: Option<&str>) -> anyhow::Result<()> {
    match returned {
        Some(s) if s == expected => Ok(()),
        Some(_) => anyhow::bail!("OAuth state mismatch — possible CSRF; aborting auth"),
        None => anyhow::bail!("no `state` in Monzo redirect — aborting auth"),
    }
}
```

> Note: if `cargo build` reports `getrandom::fill` is not found, the in-tree version exposes the older `getrandom::getrandom(&mut buf)` — use that call instead (same signature, returns `Result<(), Error>`). Verify with `cargo build` and use whichever compiles.

- [ ] **Step 5: Wire the nonce and verification into `auth_monzo`**

In `auth_monzo`, replace the line:

```rust
    let state_param = format!("aba{}", Utc::now().timestamp());
```

with:

```rust
    let state_param = random_state()?;
```

Then, in the redirect-handling section, after the existing `code` extraction and BEFORE the existing success `request.respond(...)` / token exchange, add the `state` extraction + verification. The current block is:

```rust
    let code = parsed
        .query_pairs()
        .find(|(k, _)| k == "code")
        .map(|(_, v)| v.into_owned())
        .context("no `code` in redirect")?;
    let _ = request.respond(tiny_http::Response::from_string(
        "Monzo authorisation received. You can close this tab and return to the terminal.",
    ));
```

Replace it with:

```rust
    let code = parsed
        .query_pairs()
        .find(|(k, _)| k == "code")
        .map(|(_, v)| v.into_owned())
        .context("no `code` in redirect")?;
    let returned_state = parsed
        .query_pairs()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.into_owned());
    if let Err(e) = verify_state(&state_param, returned_state.as_deref()) {
        let _ = request.respond(tiny_http::Response::from_string(
            "Authorisation rejected: state mismatch. Return to the terminal.",
        ));
        return Err(e);
    }
    let _ = request.respond(tiny_http::Response::from_string(
        "Monzo authorisation received. You can close this tab and return to the terminal.",
    ));
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test verify_state random_state`
Expected: PASS (4 tests).

- [ ] **Step 7: Confirm `Utc` is still used (no unused-import regression)**

`Utc` is used elsewhere in `run.rs` (e.g. `state.last_run = Some(Utc::now())`), so removing it from the `state_param` line does NOT make the import unused. Confirm with:

Run: `cargo clippy --all-targets -- -D warnings`
Expected: clean (no `unused_imports`).

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml Cargo.lock src/run.rs
git commit -m "fix(auth): verify OAuth state with a random nonce (CSRF)"
```

---

## Task 2: Variable-amount schedules

**Files:**
- Modify: `actual-shim/index.js` (line ~34)
- Modify: `src/actual/shim_protocol.rs` (`ScheduleRow.amount`; test ~line 70)
- Modify: `src/matching.rs` (`Schedule.amount`; `matches_scheduled`; tests)
- Modify: `src/run.rs` (`schedules_from_rows` — no code change needed; verify)
- Test: `src/matching.rs`, `src/actual/shim_protocol.rs`

- [ ] **Step 1: Write the failing matching tests**

In `src/matching.rs` `mod tests`, FIRST update the four existing `Schedule` literals to wrap the amount in `Some(...)`:
- `matches_scheduled_within_tolerance` (line ~90): `amount: Some(-1500),`
- `no_match_when_amount_differs` (line ~101): `amount: Some(-1500),`
- `no_match_when_date_out_of_tolerance` (line ~112): `amount: Some(-1500),`
- `no_match_when_payee_unrelated` (line ~123): `amount: Some(-1500),`

Then ADD two new tests:

```rust
#[test]
fn variable_amount_matches_on_date_and_payee_regardless_of_amount() {
    let schedules = vec![Schedule {
        amount: None, // variable / ranged schedule
        date: d(2026, 6, 1),
        payee: "Netflix".into(),
    }];
    // Any amount near the date with a matching payee should be treated as the
    // schedule's manifestation and skipped.
    let t = tx(-9999, d(2026, 5, 30), "NETFLIX.COM");
    assert!(matches_scheduled(&t, &schedules, 3, 0.85));
}

#[test]
fn variable_amount_still_requires_date_and_payee() {
    let schedules = vec![Schedule {
        amount: None,
        date: d(2026, 6, 1),
        payee: "Netflix".into(),
    }];
    // Wrong payee -> no match even though amount is ignored.
    let wrong_payee = tx(-9999, d(2026, 6, 1), "AMAZON PRIME");
    assert!(!matches_scheduled(&wrong_payee, &schedules, 3, 0.85));
    // Out-of-tolerance date -> no match.
    let wrong_date = tx(-9999, d(2026, 6, 10), "NETFLIX.COM");
    assert!(!matches_scheduled(&wrong_date, &schedules, 3, 0.85));
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p actual-budget-automation matches_scheduled variable_amount`
(or simply `cargo test matching`)
Expected: FAIL to compile — `Schedule.amount` is `i64`, so `Some(-1500)` and `None` are type errors.

- [ ] **Step 3: Change `Schedule.amount` to `Option<i64>` and update matching**

In `src/matching.rs`, change the struct field (line ~34):

```rust
pub struct Schedule {
    pub amount: Option<i64>,
    pub date: NaiveDate,
    pub payee: String,
}
```

Replace the `matches_scheduled` body (lines ~47-51) with:

```rust
    schedules.iter().any(|s| {
        let date_ok = (s.date - tx.date).num_days().abs() <= date_tolerance_days;
        let payee_ok = payee_similarity(&s.payee, &tx.raw_merchant) >= payee_threshold;
        let amount_ok = match s.amount {
            Some(a) => a == tx.amount_minor,
            None => true, // variable/ranged schedule: ignore amount
        };
        amount_ok && date_ok && payee_ok
    })
```

Also update the doc comment above `matches_scheduled` to note variable schedules match on date+payee only.

- [ ] **Step 4: Change `ScheduleRow.amount` to `Option<i64>`**

In `src/actual/shim_protocol.rs` (line ~38):

```rust
pub struct ScheduleRow {
    pub date: Option<String>,
    #[serde(default)]
    pub amount: Option<i64>,
    pub payee_id: Option<String>,
}
```

`#[serde(default)]` makes an absent field deserialize to `None` (a JSON `null` already does).

- [ ] **Step 5: Verify `schedules_from_rows` still compiles**

`src/run.rs:98-102` does `amount: r.amount`, now `Option<i64> -> Option<i64>`. No change needed. (If the compiler complains, it would be here.)

- [ ] **Step 6: Update the shim_protocol deserialization test**

In `src/actual/shim_protocol.rs` `mod tests`, the existing `schedule_row_accepts_null_date` test sends `"amount": 0`. Update its assertion to expect `Some(0)` for that field, and ADD a test for a null amount:

```rust
#[test]
fn schedule_row_accepts_null_amount() {
    let row: ScheduleRow = serde_json::from_value(serde_json::json!({
        "date": "2026-06-01", "amount": null, "payee_id": "p1"
    }))
    .unwrap();
    assert_eq!(row.amount, None);
}

#[test]
fn schedule_row_numeric_amount_is_some() {
    let row: ScheduleRow = serde_json::from_value(serde_json::json!({
        "date": "2026-06-01", "amount": -1500, "payee_id": "p1"
    }))
    .unwrap();
    assert_eq!(row.amount, Some(-1500));
}
```

For the existing `schedule_row_accepts_null_date` test, change its amount assertion (if it asserts on amount) to `Some(0)`; if it only asserts on `date`, leave it.

- [ ] **Step 7: Update the shim to emit `null` for non-numeric amounts**

In `actual-shim/index.js`, change the `get_scheduled` mapping (line ~34) from:

```js
        amount: typeof s._amount === 'number' ? s._amount : (s._amount && s._amount.num1) || 0,
```

to:

```js
        amount: typeof s._amount === 'number' ? s._amount : null,
```

- [ ] **Step 8: Run the tests**

Run: `cargo test matching` then `cargo test shim_protocol`
Expected: PASS — updated fixed-amount tests, two new variable-amount tests, and the new/updated `ScheduleRow` amount tests.

- [ ] **Step 9: Commit**

```bash
git add actual-shim/index.js src/actual/shim_protocol.rs src/matching.rs src/run.rs
git commit -m "fix(schedules): skip variable-amount schedules via date+payee match"
```

---

## Task 3: Truthful dry-run "would hold" count

**Files:**
- Modify: `src/run.rs` (add `would_hold_count` helper; use it in the `if dry_run` block ~lines 362-365)
- Test: `src/run.rs` (`mod tests`)

- [ ] **Step 1: Write the failing test for the helper**

The helper counts proposed+unmatched entries whose `external_id` is NOT already in `state.pending_reviews` — matching the live `add_pending*` "skip if already present" semantics. Add this test to `src/run.rs` `mod tests`. Note the module already has `use super::*;` plus `PendingReview`, `RawTransaction`, and `Confidence` (from `crate::claude`) in scope, and a local `tx(id, merchant, amount)` helper — so use bare names, do NOT add new `use` lines. `PendingReview` does **not** derive `Default`, so construct it with all fields explicitly:

```rust
#[test]
fn would_hold_count_excludes_already_pending() {
    let mut state = State::default();
    // external_id "2" is already held on disk.
    state.pending_reviews.push(PendingReview {
        external_id: "2".into(),
        raw_merchant: "Existing".into(),
        amount: -100,
        date: "2026-06-01".into(),
        resolve_to: String::new(),
        create_new: false,
        suggestion: None,
        confidence: None,
        telegram_message_id: None,
    });

    let outcome = MatchOutcome {
        matched: vec![],
        // proposed external_id "1" is new; "2" already pending.
        proposed: vec![
            (tx("1", "Cafe", -200), "Local Cafe".to_string(), Confidence::Medium),
            (tx("2", "Existing", -100), "Existing".to_string(), Confidence::Medium),
        ],
        // unmatched external_id "3" is new.
        unmatched: vec![tx("3", "Mystery", -50)],
    };

    // New holds: "1" and "3" => 2. "2" is already pending => excluded.
    assert_eq!(would_hold_count(&outcome, &state), 2);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test would_hold_count`
Expected: FAIL to compile — `would_hold_count` not defined.

- [ ] **Step 3: Add the helper**

Add to `src/run.rs` (near the other free helpers, e.g. above `run` or near `plan_resolution`):

```rust
/// Count proposed + unmatched entries that a real run would NEWLY hold — i.e.
/// excluding any whose `external_id` is already in `pending_reviews`. Mirrors the
/// live `add_pending*` "skip if already present" rule so the dry-run figure is
/// truthful. Read-only: does not mutate state.
fn would_hold_count(outcome: &MatchOutcome, state: &State) -> usize {
    let existing: std::collections::BTreeSet<&str> = state
        .pending_reviews
        .iter()
        .map(|p| p.external_id.as_str())
        .collect();
    let proposed_new = outcome
        .proposed
        .iter()
        .filter(|(tx, _, _)| !existing.contains(tx.external_id.as_str()))
        .count();
    let unmatched_new = outcome
        .unmatched
        .iter()
        .filter(|tx| !existing.contains(tx.external_id.as_str()))
        .count();
    proposed_new + unmatched_new
}
```

- [ ] **Step 4: Use the helper in the dry-run block**

In `src/run.rs`, replace the dry-run "would hold" print (lines ~362-365):

```rust
        println!(
            "  would hold   : {}",
            outcome.proposed.len() + outcome.unmatched.len()
        );
```

with:

```rust
        println!("  would hold   : {}", would_hold_count(&outcome, &state));
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test would_hold_count`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/run.rs
git commit -m "fix(run): dry-run would-hold count excludes already-pending entries"
```

---

## Task 4: Full verification gate

**Files:** none (verification only).

- [ ] **Step 1: Clippy gate**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: zero warnings/errors. No unused imports, no `dead_code` (all helpers are wired).

- [ ] **Step 2: Full test suite**

Run: `cargo test`
Expected: all tests pass — the new OAuth, schedule, and dry-run tests plus every pre-existing test (was 60 before this branch; expect 60 + the new tests).

- [ ] **Step 3: Build sanity**

Run: `cargo build`
Expected: clean.

- [ ] **Step 4: Final commit (only if verification required a fix)**

```bash
git add -A
git commit -m "chore: clippy/test cleanup for phase2 hardening"
```

---

## Self-Review

**Spec coverage:**
- Item 1 OAuth state: random nonce (`random_state`, Task 1 Step 4), verify before token exchange (`verify_state` + wiring, Step 5), pure helper unit-tested (Step 2), browser failure message (Step 5), `getrandom` dep (Step 1). ✓
- Item 2 variable schedules: shim emits `null` (Task 2 Step 7), `Option<i64>` through `ScheduleRow` (Step 4) and `Schedule` (Step 3), date+payee-only match for `None` (Step 3), tests incl. variable match + variable no-match + deserialization (Steps 1, 6). ✓
- Item 3 dry-run count: `would_hold_count` excludes already-pending (Task 3 Steps 3-4), read-only (no `add_pending*`), unit-tested with a pre-existing hold (Step 1). ✓
- Cross-cutting verification gate: Task 4. ✓

**Placeholder scan:** No TBD/TODO. The two "if the compiler complains / use whichever resolves" notes (getrandom API, `tx` helper path) are deliberate fallbacks with the exact alternative given, not placeholders.

**Type consistency:** `Schedule.amount: Option<i64>` and `ScheduleRow.amount: Option<i64>` consistent; `schedules_from_rows` pass-through unchanged; `matches_scheduled(&RawTransaction, &[Schedule], i64, f64) -> bool` signature unchanged; `verify_state(&str, Option<&str>) -> anyhow::Result<()>`, `random_state() -> anyhow::Result<String>`, `would_hold_count(&MatchOutcome, &State) -> usize` referenced consistently between their tests and definitions. ✓
