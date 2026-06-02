# Phase 2 Hardening — Design

**Date:** 2026-06-02
**Status:** Approved for planning
**Phase 2 backlog item:** #5 (see `docs/PHASE2-BACKLOG.md`)

## Overview

Three small, independent correctness/security fixes from the Phase 1 review,
bundled as one cycle because each is too small for its own. None changes the
overall architecture; all ship on a single `feat/phase2-hardening` branch as
three separate commits/tasks.

1. **OAuth `state` (CSRF) verification** in the Monzo auth bootstrap.
2. **Variable-amount schedules** no longer silently degrade to `0` (which caused
   scheduled transactions to be wrongly imported instead of skipped).
3. **Dry-run "would hold" count** matches what a real run would actually hold.

## Goals

- Close the OAuth CSRF gap: a forged/mismatched redirect must be rejected before
  any token exchange, using an unguessable `state`.
- Stop importing manifestations of variable/ranged schedules: a schedule with a
  non-fixed amount must still skip its real transaction.
- Make the dry-run hold count truthful (no overstatement when holds already
  exist).

## Non-goals (this cycle)

- Unit-testing the full `auth_monzo` orchestration (browser-open + socket bind +
  token exchange). Only the new pure `state`-verification helper is tested, as is
  consistent with the rest of that function today.
- Range-precise schedule matching (skip only if amount ∈ [num1, num2]). Decided
  against: variable schedules match on date+payee only. (Recorded as a possible
  future refinement, not built.)
- Any change to the import/dedup path, `last_run` semantics, or pagination.
- The other Phase 2 items (#2 blocked, #4, #6, #7).

---

## Item 1 — OAuth `state` verification

**Files:** `src/run.rs` (`auth_monzo`), `Cargo.toml`.

**Problem.** `auth_monzo` (`src/run.rs:452-515`) generates
`state_param = format!("aba{}", Utc::now().timestamp())`, sends it in the auth
URL, but the redirect handler extracts only `code` and never inspects the
returned `state`. The value is also predictable (wall-clock seconds).

**Design.**
- **Unguessable nonce.** Replace the timestamp value with a random 128-bit nonce,
  hex-encoded to a 32-char string. Add `getrandom` as a direct dependency (it is
  already in the tree transitively via rustls; tiny, `no_std`). Fill `[u8; 16]`
  via `getrandom::getrandom(&mut buf)`, then hex-encode with
  `buf.iter().map(|b| format!("{b:02x}")).collect::<String>()` (no extra hex
  crate). On RNG failure, return an error (the bootstrap cannot proceed safely
  without a real nonce).
- **Verify on redirect.** After parsing the redirect URL, extract the `state`
  query param exactly as `code` is extracted. Verify it against the generated
  nonce **before** the token exchange.
- **Pure, testable helper.** Add:

  ```rust
  /// Verify the OAuth `state` echoed back on the redirect matches what we sent.
  fn verify_state(expected: &str, returned: Option<&str>) -> anyhow::Result<()>
  ```

  Returns `Ok(())` only when `returned == Some(expected)`; otherwise
  `anyhow::bail!` with a message distinguishing "missing" from "mismatch". The
  monolithic `auth_monzo` calls this; the orchestration itself stays untested
  (unchanged from today).
- **Browser feedback.** On verification failure, respond to the still-open
  request with a short failure string before returning the error, so the user
  isn't left staring at a hung tab.

**Error handling.** Mismatch/missing `state` → `Err` from `auth_monzo` (the
process exits non-zero, no tokens written). RNG failure → `Err`.

**Testing.** Unit tests on `verify_state`: (a) `verify_state("abc", Some("abc"))`
is `Ok`; (b) `verify_state("abc", Some("xyz"))` is `Err`; (c)
`verify_state("abc", None)` is `Err`. No network/browser in tests.

---

## Item 2 — Variable-amount schedules

**Files:** `actual-shim/index.js`, `src/actual/shim_protocol.rs` (`ScheduleRow`),
`src/matching.rs` (`Schedule`, `matches_scheduled`), `src/run.rs`
(`schedules_from_rows`).

**Problem.** `matches_scheduled` (`src/matching.rs:47-51`) requires an **exact**
`s.amount == tx.amount_minor`. The shim
(`actual-shim/index.js:34`) currently coerces any non-number `_amount` via
`(s._amount && s._amount.num1) || 0`, so a recurrence/range object lacking
`num1` yields `0`. No real transaction has amount `0`, so the schedule never
matches → its real transaction is **imported instead of skipped** (false
negative).

**Design.**
- **Shim emits `null` for non-fixed amounts.** Change line 34 to:

  ```js
  amount: typeof s._amount === 'number' ? s._amount : null,
  ```

  i.e. fixed number → that number; anything else (range/recurrence object,
  missing) → `null`. The misleading `num1`/`|| 0` fallback is removed.
- **Make amount optional through the pipeline.**
  - `ScheduleRow.amount: i64` → `Option<i64>` (`src/actual/shim_protocol.rs:38`).
    With serde, a JSON `null` or absent field deserializes to `None`. Add
    `#[serde(default)]` so an absent field is also tolerated.
  - `Schedule.amount: i64` → `Option<i64>` (`src/matching.rs:34`).
  - `schedules_from_rows` (`src/run.rs:98-102`) passes `amount: r.amount`
    unchanged (now `Option → Option`).
- **Matching semantics.** `matches_scheduled` per schedule:
  - `Some(amt)` → unchanged: `amt == tx.amount_minor` **and** date within
    tolerance **and** payee similarity ≥ threshold.
  - `None` → date within tolerance **and** payee similarity ≥ threshold (amount
    ignored). A variable schedule thus skips any near-date tx with a matching
    payee — the accepted trade-off.

**Testing (`src/matching.rs`).**
- Existing fixed-amount tests updated to wrap amounts in `Some(...)` and must
  still pass (exact match, no-match-on-amount, no-match-on-date, no-match-on-payee).
- New: a `Schedule { amount: None, .. }` matches a tx of an arbitrary amount when
  date+payee align (proves variable skip works).
- New: a `Schedule { amount: None, .. }` does **not** match when the payee
  differs or the date is out of tolerance (proves it isn't over-greedy).
- `shim_protocol.rs`: `ScheduleRow` deserializes `amount: null` → `None` and a
  numeric `amount` → `Some(n)`. (The existing `schedule_row_accepts_null_date`
  test currently sends `"amount": 0`; add/adjust a case for `"amount": null`.)

---

## Item 3 — Dry-run "would hold" count

**Files:** `src/run.rs` (the `if dry_run` block, around lines 355-365).

**Problem.** The live path increments `held` only when `add_pending*` returns
`true` — i.e. the entry was newly added, not already in `pending_reviews`
(`src/run.rs:343`, `:350`). The dry-run block instead prints
`outcome.proposed.len() + outcome.unmatched.len()` (`:362-364`), which counts
**all** of them, overstating the figure whenever holds already exist on disk.

**Design.** Compute the dry-run figure with the same "not already pending"
predicate the live counter uses: count entries in `outcome.proposed` and
`outcome.unmatched` whose `external_id` is **not** already present in
`state.pending_reviews`. Use whatever membership check the live `add_pending*`
relies on (a set/lookup over `pending_reviews` keyed by `external_id`).

**Constraint.** Dry-run must remain read-only — compute the count without
mutating `state` (no `add_pending*` calls). Build a `BTreeSet<&str>` (or reuse an
existing helper) of existing pending `external_id`s and count the
not-yet-present proposed+unmatched entries.

**Testing (`src/run.rs`).** Add a dry-run test where one of the proposed/unmatched
entries is **already** in `pending_reviews`: assert the printed/returned
would-hold count excludes it. (If the count is only printed, factor the
computation into a small pure helper — e.g. `fn would_hold_count(outcome, state)
-> usize` — and unit-test that, rather than scraping stdout.)

---

## Cross-cutting

- **Branch:** `feat/phase2-hardening`, three commits (one per item), TDD.
- **Verification gate (per project memory):** after each item and at the end, run
  `cargo build`, `cargo clippy --all-targets -- -D warnings`, `cargo test` and
  read the **actual** output. `aba` is a binary crate — use `cargo test`, never
  `cargo test --lib`. Treat `<new-diagnostics>` as advisory only.
- **No silent behavior change to the happy path:** fixed-amount schedules, valid
  OAuth redirects, and dry-runs with no pre-existing holds all behave exactly as
  before.
