# `run()` Integration Coverage — Design

**Date:** 2026-06-02
**Status:** Approved for planning
**Phase 2 backlog item:** #4 (see `docs/PHASE2-BACKLOG.md`)

## Overview

`run()` (the nightly fetch → match → import orchestrator) gained its first
integration tests in Phase 1 Task 10 (held→confirm→import, dry-run no-writes,
idempotent re-prompt). Three highest-risk paths remain uncovered. This adds the
test doubles needed to exercise them and four focused tests. **No production code
changes** — this is purely test coverage (plus one backlog doc correction).

## Goals

- Cover the **AuthExpired early-return** path: re-auth notification + state save
  fire only when `!dry_run`, and the run exits cleanly without fetching/importing.
- Lock in **dry-run state-splice correctness**: a dry run leaves `payee_cache`,
  `pending_reviews`, and `last_run` untouched on disk and persists **only** the
  rotated Monzo refresh token.
- Characterize the **create_payee→import failure** path: the run fails loudly
  (`Err`) and the resolved review survives on disk, so a retry is clean (no silent
  data loss). This documents the self-healing property rather than changing it.

## Non-goals (this cycle)

- Any production change to `run()`, `auth_monzo`, the shim, or state handling.
- Refactoring the create_payee→import path to eliminate the transient orphan
  payee (atomic `payee_name` auto-create). Decided against for #4; overlaps #7 and
  can be revisited there.
- A stateful two-run shim demonstrating the full create→reuse self-heal. Test 4
  proves the load-bearing property (resolved review survives a failed import →
  retry possible); the reuse-not-duplicate behavior follows from the existing
  fresh-`get_payees` + `payee_id_for` lookup and is not re-proven here.

## Background: confirmed behavior (from reading `src/run.rs`)

- **AuthExpired** (`run.rs:176-189`): on `ensure_auth → AuthExpired`, the code
  `warn!`s, and **only when `!dry_run`** calls `notifier.notify("Monzo re-auth
  required", ...)` then `state.save(...)`, then `return Ok(())`. `fetch_since` and
  everything after are skipped. (The backlog's "save is always attempted" is
  inaccurate; it is gated on `!dry_run`. This spec treats the code as correct and
  corrects the backlog.)
- **Dry-run splice** (`run.rs` dry-run block): prints a report, then re-loads the
  on-disk state and writes back **only** `monzo.refresh_token` (and `account_id`
  if it was empty), discarding in-memory cache/pending/`last_run` changes;
  `last_run` is never advanced on a dry run.
- **Resolved-review import** (`run.rs:235`, `:330-353`, `:398-402`, `:429`):
  `take_resolved()` removes resolved entries **in memory** and folds them into the
  cache; `create_payee` runs for `create_new` entries; the batch
  `import_transactions` runs; `state.save()` happens **only after** a successful
  import. So an import failure returns `Err` via `?` before `save`, leaving the
  on-disk resolved review intact → a clean retry.
- A pending review is "resolved" (`state.rs:90-122`) when `create_new == true`
  **or** `resolve_to` is non-empty.

## New test doubles (in `src/run.rs` `mod tests`)

Added next to the existing Task 10 doubles; existing doubles and tests are
untouched.

- **`RecordingNotifier`** — `{ calls: Mutex<Vec<(String, String)>> }`; `notify`
  pushes `(title.into(), body.into())`. Lets tests assert a notification fired (or
  did not) and inspect its title. (`NullNotifier` stays for tests that don't care.)
- **`AuthExpiredBank`** — `ensure_auth` returns
  `Err(BankError::AuthExpired("test".into()))`; `fetch_since` returns
  `Ok(vec![])` (never reached). `name()` → `"fake"`.
- **`RotatingBank`** — `{ rotate_to: String }`; `ensure_auth` sets
  `state.monzo.refresh_token = self.rotate_to.clone()` and returns `Ok(())`
  (simulates Monzo's refresh-token rotation); `fetch_since` returns `Ok(vec![])`.
- **`fake_actual_failing_import(dir)`** — like `fake_actual` but the shim's
  `import_transactions` branch returns `{ok:false,error:'boom'}`, and its
  `create_payee` branch appends to `<dir>/payees.log` before returning
  `{id:'p_new'}` (so the test can assert `create_payee` was attempted). `get_payees`
  / `get_scheduled` behave as in `fake_actual`.

## Tests

All in `src/run.rs` `mod tests`, `#[tokio::test]` unless noted, using the existing
`cfg_at` / `tempdir` patterns.

1. **`auth_expired_notifies_and_saves_when_not_dry_run`** — `AuthExpiredBank`,
   `RecordingNotifier`, `fake_actual`, `run(dry_run=false)`. Assert: returns `Ok`;
   exactly one notify call whose title == `"Monzo re-auth required"`; the state
   file exists on disk (save happened); `imports.log` does **not** exist (no
   fetch/import).
2. **`auth_expired_dry_run_does_not_notify_or_save`** — same with
   `run(dry_run=true)`. Assert: returns `Ok`; `RecordingNotifier.calls` is empty;
   the state file was **not** written (path does not exist).
3. **`dry_run_persists_only_rotated_token`** — write an on-disk baseline state with
   `payee_cache = {"RAW"->"Payee"}`, one `pending_review`, `last_run = Some(t)`,
   `monzo.refresh_token = "old"`, `monzo.account_id = "acc"`; load it into `state`;
   `RotatingBank { rotate_to: "new" }`; `run(dry_run=true)`. Reload the on-disk
   state and assert: `payee_cache`, `pending_reviews`, and `last_run` equal the
   baseline (unchanged), and `monzo.refresh_token == "new"`.
4. **`create_payee_import_failure_leaves_review_for_retry`** — seed `state` with a
   single `create_new` resolved review (`create_new: true`, `resolve_to: ""`,
   `suggestion: None` → payee name falls back to `raw_merchant`), set
   `monzo.account_id`, and write it to disk as the baseline; use
   `fake_actual_failing_import`; `run(dry_run=false)`. Assert: returns `Err`
   (context `"import_transactions"`); reloading the on-disk state still contains
   the pending review (retry is possible); `payees.log` exists (create_payee was
   attempted — documents the transient orphan).

## Documentation change

- `docs/PHASE2-BACKLOG.md` #4: correct the AuthExpired bullet from "save is always
  attempted" to "notification **and** save both fire only when `!dry_run`; the run
  then early-returns `Ok`." Mark #4 done after implementation.

## Cross-cutting

- **Branch:** `feat/phase2-run-coverage`. Task 1 adds the four doubles; Tasks 2-5
  add one test each (commit per task); Task 6 is the verification gate.
- **Verification (project memory):** `cargo build`, `cargo clippy --all-targets --
  -D warnings`, `cargo test` — read the actual output. Binary crate → `cargo
  test`, never `--lib`. `<new-diagnostics>` are advisory only.
- These are additive tests; all pre-existing tests (68) must still pass.
