# Review Remediation Phase B — `run.rs` Decomposition Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Break the 4,351-line `src/run.rs` (≈1,486 LOC code + ≈2,865 LOC tests, dominated by one 626-line `run()`) into a focused `pipeline/` module tree plus `telegram/prompts.rs` and `report.rs` — **without changing any behaviour**.

**Architecture:** This is the *aggressive* decomposition (chosen 2026-06-10): we not only relocate the standalone helpers, types, and tests into cohesive modules, we also extract `run()`'s in-line phases (Flex-mirror reconciliation, transfer-intent resolution, import-row assembly) into named functions so `run()` itself shrinks to a thin orchestrator (~120 lines). The existing ≈2,865 LOC test suite — especially the ~24 `run()` integration tests — is the safety net: every task ends with the full suite green, so any mis-wiring of the extracted phases turns a test red immediately. We rely on that net rather than rewriting tests.

**Tech Stack:** Rust edition 2024 (clippy pedantic+nursery via `[lints.clippy]`), tokio, reqwest, serde/toml, tiny_http (dev OAuth only, `auth-flow` feature), Node shim (`actual-shim/index.js`, `@actual-app/api`).

---

## Target file structure (end state)

| File | Responsibility | Public items (crate-internal) |
|------|----------------|-------------------------------|
| `src/pipeline/mod.rs` | `run()` orchestrator + module wiring + re-exports | `run`, re-exports `map_accounts`, `auth_monzo` |
| `src/pipeline/auth.rs` | Monzo token persistence + OAuth bootstrap | `persist_rotated_token`; `auth_monzo`/`random_state`/`verify_state`/`urlencoding` (cfg `auth-flow`) |
| `src/pipeline/transfer.rs` | Transfer routing + Flex reconciliation | `actual_id_for`, `transfer_payee_id`, `resolve_transfer_dest`, `reconcile_flex_mirrors`, `resolve_intents` |
| `src/pipeline/classify.rs` | Cache split + Claude gating | `plan_resolution`, `apply_matches`, `category_id_for`, `payee_id_for`, `ResolutionPlan`, `HeldClassification`, `MatchOutcome` |
| `src/pipeline/import.rs` | Schedule build + import-row assembly | `schedules_from_rows`, `hold_unmapped`, `assemble`, `AssembleInputs`, `Assembled` |
| `src/pipeline/accounts.rs` | `aba accounts` command | `map_accounts`, `suggest_links`, `suggest_links_with_flex`, `suggest_pot_links`, `map_bank_err`, `bank_err_text` |
| `src/pipeline/testutil.rs` | `#[cfg(test)]` shared fakes/builders | `FakeChannel`, `FakeBank`, `FakeClaude`, `NullNotifier`, `RecordingNotifier`, `AuthExpiredBank`, `RotatingBank`, `tx`, `cls`, `held`, `cfg_at`, `fake_actual*`, `pot_deposit` |
| `src/telegram/mod.rs` | (was `src/telegram.rs`) Telegram client + protocol | unchanged surface |
| `src/telegram/prompts.rs` | Prompt building + group send | `build_prompt`, `rehold_failed_remaps`, `send_group_prompts` |
| `src/report.rs` | Dry-run report + summary body | `dry_run`, `DryRunCounts`, `summary_body` |

---

## Conventions (apply to EVERY task)

1. **No behaviour change.** This is a pure refactor. The only intentional content additions are the two fold-in comments in Task 8. Moved code is byte-identical except for the visibility/import edits each task specifies.
2. **Visibility rule (avoids the nursery `redundant_pub_crate` trap).** When an item moves into a *private* submodule (`mod classify;`, not `pub mod`) and is used by a sibling module, mark it **`pub`** — NOT `pub(crate)`. In a binary crate a `pub` item in a private module is already only crate-visible, and `pub` does not trip clippy nursery's `redundant_pub_crate` (which fires on `pub(crate)`). This matches the existing `pub fn encode_callback` in `telegram.rs`. Items already declared `pub` keep it; items currently private that gain a cross-module caller become `pub`.
3. **Test fixtures come from `testutil`.** After Task 2, unit tests in submodules and integration tests in `pipeline/mod.rs` import shared fakes/builders via `use crate::pipeline::testutil::*;` (or `use super::testutil::*;` from `mod.rs`). Each submodule's own `#[cfg(test)] mod tests` adds `use super::*;` plus the `testutil` import as needed.
4. **Each `#[cfg(test)] mod tests` block** that moves into a submodule keeps the test functions verbatim; only the `use` header changes (point at `super::*` for the moved fns + `crate::pipeline::testutil::*` for fakes).
5. **Verification gate — run in THIS order after every task; never infer success from a piped exit code, grep output for `error[` / `FAILED` / `warning:`):**
   ```bash
   cargo fmt --all
   cargo clippy --all-targets                              # warn pass (matches edit hook)
   cargo clippy --all-targets -- -D warnings
   cargo clippy --all-targets --no-default-features -- -D warnings
   cargo test
   cargo test --no-default-features
   cargo fmt --all -- --check
   ```
   (`--no-default-features` matters from Task 7 on, where the `auth-flow` gate lives; running it throughout is cheap insurance.)
6. **One commit per task.** Conventional-commit subject given in each task's final step.
7. **Line numbers** below are against the current `src/run.rs` (HEAD = `2582ce9`). After Task 1 the file is `src/pipeline/mod.rs`; the same line numbers still apply until edited.

---

### Task 1: Create the `pipeline/` module skeleton (rename, no logic change)

**Files:**
- Rename: `src/run.rs` → `src/pipeline/mod.rs`
- Modify: `src/main.rs:10` and `:90,:147,:164` (module decl + call sites)
- Modify: `src/poll.rs:112,:291,:335` (test fixture path)

- [ ] **Step 1: Move the file with git (preserves history)**

```bash
mkdir src/pipeline
git mv src/run.rs src/pipeline/mod.rs
```

- [ ] **Step 2: Repoint the module declaration and call sites in `main.rs`**

In `src/main.rs` change line 10 from `mod run;` to `mod pipeline;`.
Then update the three call sites:
- `:90`  `run::auth_monzo(cfg).await` → `pipeline::auth_monzo(cfg).await`
- `:147` `run::run(` → `pipeline::run(`
- `:164` `run::map_accounts(cfg, write).await` → `pipeline::map_accounts(cfg, write).await`

(`map_accounts` and `auth_monzo` are still defined directly in `pipeline/mod.rs` at this point, so `pipeline::map_accounts` / `pipeline::auth_monzo` resolve. Re-exports get added in Tasks 6 and 7.)

- [ ] **Step 3: Repoint the cross-module test fixture path in `poll.rs`**

In `src/poll.rs` replace all three occurrences of `crate::run::tests::FakeChannel` (lines 112, 291, 335) with `crate::pipeline::tests::FakeChannel`.

- [ ] **Step 4: Run the verification gate**

Expected: identical test count to HEAD, all green. This commit is a pure relocation.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "refactor(pipeline): rename run.rs to pipeline/mod.rs (no logic change)"
```

---

### Task 2: Extract shared test fixtures to `pipeline/testutil.rs`

**Files:**
- Create: `src/pipeline/testutil.rs`
- Modify: `src/pipeline/mod.rs` (add `mod testutil;` decl; trim moved items from `mod tests`; add import)
- Modify: `src/poll.rs:112,:291,:335` (fixture path)

- [ ] **Step 1: Declare the test-only module in `pipeline/mod.rs`**

Add near the other `mod` lines at the top of `src/pipeline/mod.rs` (after the `use` block, before `const ACCOUNT_MATCH_THRESHOLD`):
```rust
#[cfg(test)]
pub mod testutil;
```
(`pub` so `poll.rs` tests can reach it; the module is `#[cfg(test)]` so it never ships. Per Convention 2, `pub` — not `pub(crate)` — to dodge `redundant_pub_crate`.)

- [ ] **Step 2: Move the shared fixtures into `src/pipeline/testutil.rs`**

Create `src/pipeline/testutil.rs` with a `use` header that brings in everything the fixtures reference, then move these items **verbatim** out of `pipeline/mod.rs`'s `mod tests` (current `src/run.rs` line ranges):

| Item | Lines |
|------|-------|
| `fn tx` (RawTransaction builder) | 1501–1510 |
| `struct FakeChannel` + `impl FakeChannel` + `impl ConfirmChannel for FakeChannel` | 1512–1573 |
| `fn cls` (Classification builder) | 1627–1641 |
| `fn held` (PendingReview builder) | 1969–1986 |
| `struct FakeBank` + `impl BankSource for FakeBank` | 2089–2106 |
| `struct FakeClaude` + `impl ClaudeMatcher for FakeClaude` | 2108–2121 |
| `struct NullNotifier` + impl | 2123–2129 |
| `struct RecordingNotifier` + impls | 2131–2151 |
| `struct AuthExpiredBank` + impl | 2153–2169 |
| `struct RotatingBank` + impl | 2171–2190 |
| `fn fake_actual` | 2192–2219 |
| `fn fake_actual_failing_import` | 2221–2247 |
| `fn fake_actual_failing_get_payees` | 2249–2268 |
| `fn fake_actual_routing` | 2270–2297 |
| `fn fake_actual_transfers` | 2299–2325 |
| `fn cfg_at` | 2327–2341 |
| `fn pot_deposit` (RawTransaction builder for pot tests) | 2536–2547 |

The file header:
```rust
//! Shared test fixtures for the pipeline integration + unit tests.
#![cfg(test)]

use std::path::Path;

use chrono::{DateTime, TimeZone as _, Utc};

use crate::actual::ActualClient;
use crate::banks::{BankError, BankSource, RawTransaction, TransferTarget};
use crate::claude::{Classification, ClaudeMatcher, Confidence};
use crate::config::Config;
use crate::notify::Notifier;
use crate::state::{PendingReview, State};
use crate::telegram::{ConfirmChannel, Prompt};
```
Make every moved item `pub` (builders `pub fn tx`, fakes `pub struct FakeBank`, etc.) so they are reachable as `crate::pipeline::testutil::*`. Adjust the actual `use` list to exactly what the moved bodies reference — compile errors from the gate will pinpoint any missing/extra import; fix until green. (The exact set above is derived from the fixtures' current bodies; treat the gate as the authority.)

- [ ] **Step 3: Import the fixtures back into `mod tests`**

At the top of `pipeline/mod.rs`'s `#[cfg(test)] mod tests` block (the `use super::*;` at line 1489), add:
```rust
use super::testutil::*;
```

- [ ] **Step 4: Repoint `poll.rs` to the new fixture home**

In `src/poll.rs` replace `crate::pipeline::tests::FakeChannel` (3 sites) with `crate::pipeline::testutil::FakeChannel`.

- [ ] **Step 5: Run the gate, then commit**

Expected: same test count, all green (fixtures merely relocated).
```bash
git add -A
git commit -m "test(pipeline): hoist shared fakes/builders into testutil module"
```

---

### Task 3: Extract `pipeline/classify.rs`

**Files:**
- Create: `src/pipeline/classify.rs`
- Modify: `src/pipeline/mod.rs` (add `mod classify;` + `use`; remove moved items)

- [ ] **Step 1: Declare and create the module**

Add `mod classify;` to the module list near the top of `pipeline/mod.rs`. Create `src/pipeline/classify.rs` with header:
```rust
use std::collections::{BTreeMap, BTreeSet};

use tracing::warn;

use crate::actual::shim_protocol::{Category, Payee};
use crate::banks::RawTransaction;
use crate::claude::{Classification, Confidence};
```

- [ ] **Step 2: Move these items verbatim from `pipeline/mod.rs`**

| Item | Lines | Visibility after move |
|------|-------|----------------------|
| `struct ResolutionPlan` | 75–84 | `pub` (already) |
| `fn plan_resolution` | 86–112 | `pub` (already) |
| `struct HeldClassification` | 114–128 | `pub` (already) |
| `struct MatchOutcome` | 130–139 | `pub` (already) |
| `enum CatState` | 141–145 | private (stays — internal to `apply_matches`) |
| `fn apply_matches` (with its `#[allow(clippy::similar_names)]` + the 2-line comment above it, 147–149) | 147–242 | `pub` (already) |
| `fn category_id_for` | 244–255 | `private → pub` |
| `fn payee_id_for` | 307–313 | `private → pub` |

- [ ] **Step 3: Move the classify unit tests into `classify.rs`**

Append to `classify.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::testutil::*;
    // moved test fns below
}
```
Move these test fns verbatim out of `mod tests` in `pipeline/mod.rs`:
- `plan_needs_claude_unless_both_caches_hit` (1600–1625)
- `apply_matches_gates_on_both_dimensions` (1643–1697)
- `apply_matches_autosettles_category_when_none_exist` (1699–1719)
- `category_id_for_holds_on_ambiguous_name` (1721–1745)

- [ ] **Step 4: Wire `pipeline/mod.rs` to the moved symbols**

`run()` calls `plan_resolution`, `apply_matches`, `category_id_for`, `payee_id_for` and names `ResolutionPlan`/`HeldClassification`/`MatchOutcome`. Add to the `use` block at the top of `pipeline/mod.rs`:
```rust
use classify::{
    MatchOutcome, ResolutionPlan, apply_matches, category_id_for, payee_id_for, plan_resolution,
};
// Re-exported (not just `use`d) because the top-level `report` module — a sibling
// of `pipeline`, not a descendant — needs `crate::pipeline::HeldClassification`,
// and `mod classify;` is private to `pipeline`. `pub use` brings it into local
// scope for `run()` AND exposes it crate-wide.
pub use classify::HeldClassification;
```
(Remove any now-unused imports the gate flags.)

- [ ] **Step 5: Run the gate, then commit**

```bash
git add -A
git commit -m "refactor(pipeline): extract classify module (cache split + gating)"
```

---

### Task 4: Extract `pipeline/transfer.rs` (routing helpers only)

**Files:**
- Create: `src/pipeline/transfer.rs`
- Modify: `src/pipeline/mod.rs` (`mod transfer;` + `use`; remove moved items)

- [ ] **Step 1: Declare and create the module**

Add `mod transfer;`. Create `src/pipeline/transfer.rs`:
```rust
use crate::actual::shim_protocol::Payee;
use crate::banks::TransferTarget;
use crate::state::{AccountLink, PotLink};
```

- [ ] **Step 2: Move these items verbatim**

| Item | Lines | Visibility |
|------|-------|-----------|
| `fn actual_id_for` | 24–41 | `private → pub` |
| `fn transfer_payee_id` | 43–50 | `private → pub` |
| `fn resolve_transfer_dest` | 52–73 | `private → pub` |

- [ ] **Step 3: Move the transfer unit tests**

Append a `#[cfg(test)] mod tests { use super::*; use crate::pipeline::testutil::*; }` and move verbatim:
- `actual_id_for_matches_source_then_falls_back_to_sole_mapped` (1575–1598)
- `transfer_payee_id_finds_the_account_transfer_payee` (4132–4149)
- `resolve_transfer_dest_maps_pot_then_account` (4151–4197)

- [ ] **Step 4: Wire `mod.rs`**

Add to `pipeline/mod.rs` `use`:
```rust
use transfer::{actual_id_for, resolve_transfer_dest, transfer_payee_id};
```

- [ ] **Step 5: Run the gate, then commit**

```bash
git add -A
git commit -m "refactor(pipeline): extract transfer routing helpers"
```

---

### Task 5: Extract `pipeline/import.rs` (schedule + hold helpers)

**Files:**
- Create: `src/pipeline/import.rs`
- Modify: `src/pipeline/mod.rs` (`mod import;` + `use`; remove moved items)

- [ ] **Step 1: Declare and create the module**

Add `mod import;`. Create `src/pipeline/import.rs`:
```rust
use std::collections::BTreeMap;

use chrono::NaiveDate;
use tracing::warn;

use crate::actual::shim_protocol::{Payee, ScheduleRow};
use crate::banks::RawTransaction;
use crate::matching::Schedule;
use crate::state::State;
```

- [ ] **Step 2: Move these items verbatim**

| Item | Lines | Visibility |
|------|-------|-----------|
| `fn schedules_from_rows` | 271–305 | `private → pub` |
| `fn hold_unmapped` | 257–270 | `private → pub` |

- [ ] **Step 3: Move the import unit test**

Append `#[cfg(test)] mod tests { use super::*; use crate::pipeline::testutil::*; }` and move verbatim:
- `schedules_from_rows_drops_bad_date_and_missing_payee_loudly` (3926–3962)

- [ ] **Step 4: Wire `mod.rs`**

Add to `pipeline/mod.rs` `use`:
```rust
use import::{hold_unmapped, schedules_from_rows};
```

- [ ] **Step 5: Run the gate, then commit**

```bash
git add -A
git commit -m "refactor(pipeline): extract import schedule + hold helpers"
```

---

### Task 6: Extract `pipeline/accounts.rs` (`aba accounts` command)

**Files:**
- Create: `src/pipeline/accounts.rs`
- Modify: `src/pipeline/mod.rs` (`mod accounts;` + re-export; remove moved items)

- [ ] **Step 1: Declare and create the module**

Add `mod accounts;`. Create `src/pipeline/accounts.rs`:
```rust
use anyhow::Context as _;
use tracing::warn;

use crate::actual::ActualClient;
use crate::banks::BankError;
use crate::config::Config;
use crate::state::{AccountLink, PotLink, State};

use super::ACCOUNT_MATCH_THRESHOLD;
use super::auth::persist_rotated_token;
```
(`persist_rotated_token` is created in Task 7; until then it still lives in `mod.rs` — for THIS task import it as `use super::persist_rotated_token;` and switch the path in Task 7. To keep tasks independent, see Step 2 note.)

> **Sequencing note:** `map_accounts` calls `persist_rotated_token` (line 621) and `map_bank_err`/`bank_err_text`. `map_bank_err`+`bank_err_text` move WITH `accounts.rs` (Step 2). `persist_rotated_token` still lives in `mod.rs` at this point, so import it via `use super::persist_rotated_token;`. Task 7 moves it to `auth.rs` and updates this one import line.

- [ ] **Step 2: Move these items verbatim**

| Item | Lines | Visibility |
|------|-------|-----------|
| `fn suggest_links` | 519–551 | `private → pub` |
| `fn suggest_links_with_flex` | 553–572 | `private → pub` |
| `fn suggest_pot_links` | 574–603 | `private → pub` |
| `fn map_accounts` (with its doc + `#[allow(clippy::future_not_send)]`, 605–609) | 605–709 | `pub` (already) |
| `fn map_bank_err` | 711–716 | `private → pub` |
| `fn bank_err_text` | 718–723 | `private → pub` |

Within `map_accounts`, the internal references to `crate::banks::monzo::Monzo`, `crate::lock`, `crate::matching::payee_similarity` (used by `suggest_*`) stay fully-qualified and resolve unchanged.

- [ ] **Step 3: Re-export `map_accounts` from `mod.rs`**

`main.rs` calls `pipeline::map_accounts`. Add to `pipeline/mod.rs`:
```rust
pub use accounts::map_accounts;
```

- [ ] **Step 4: Move the accounts unit tests**

Append `#[cfg(test)] mod tests { use super::*; use crate::pipeline::testutil::*; }` and move verbatim:
- `suggest_links_matches_by_name_and_leaves_unmatched_unmapped` (3906–3924)
- `map_accounts_flags_flex_account_by_description_prefix` (4199–4214)
- `suggest_pot_links_matches_by_name_and_leaves_unmatched_unmapped` (4216–4231)

- [ ] **Step 5: Run the gate, then commit**

```bash
git add -A
git commit -m "refactor(pipeline): extract accounts command (map_accounts + suggest helpers)"
```

---

### Task 7: Extract `pipeline/auth.rs` (token persistence + OAuth bootstrap)

**Files:**
- Create: `src/pipeline/auth.rs`
- Modify: `src/pipeline/mod.rs` (`mod auth;` + re-exports; remove moved items)
- Modify: `src/pipeline/accounts.rs` (one import line)

- [ ] **Step 1: Declare and create the module**

Add `mod auth;`. Create `src/pipeline/auth.rs`:
```rust
use crate::config::Config;
use crate::state::State;
```

- [ ] **Step 2: Move `persist_rotated_token` (always compiled)**

Move verbatim from `pipeline/mod.rs`:
| Item | Lines | Visibility |
|------|-------|-----------|
| `fn persist_rotated_token` (with its doc comment 503–509) | 503–517 | `private → pub` |

- [ ] **Step 3: Move the feature-gated OAuth items**

Move verbatim (each keeps its `#[cfg(feature = "auth-flow")]` attribute):
| Item | Lines |
|------|-------|
| `fn random_state` (1360–1369) | 1360–1369 |
| `fn verify_state` (1371–1381) | 1371–1381 |
| `async fn auth_monzo` (1383–1468) | 1383–1468 |
| `fn urlencoding` (1470–1485) | 1470–1485 |

These reference `tiny_http`, `webbrowser`, `crate::banks::monzo::Monzo`, `crate::lock`, etc. Add their imports to `auth.rs` under a `#[cfg(feature = "auth-flow")]` use block as the gate flags them, e.g.:
```rust
#[cfg(feature = "auth-flow")]
use anyhow::Context as _;
```
(Keep each `use` minimal; the `--no-default-features` clippy run will flag anything unused-under-that-config.)

- [ ] **Step 4: Re-export for `mod.rs` callers + fix the `accounts.rs` import**

In `pipeline/mod.rs`:
```rust
use auth::persist_rotated_token;
#[cfg(feature = "auth-flow")]
pub use auth::auth_monzo;
```
(`run()` calls `persist_rotated_token`; `main.rs` calls `pipeline::auth_monzo` under the feature.)

In `src/pipeline/accounts.rs` change the Task-6 import `use super::persist_rotated_token;` to `use super::auth::persist_rotated_token;`.

- [ ] **Step 5: Move the auth unit tests (feature-gated)**

Append to `auth.rs`:
```rust
#[cfg(all(test, feature = "auth-flow"))]
mod tests {
    use super::*;
    // moved fns
}
```
Move verbatim:
- `verify_state_accepts_exact_match` (3964–3968)
- `verify_state_rejects_mismatch` (3970–3974)
- `verify_state_rejects_missing` (3976–3980)
- `random_state_is_32_hex_chars_and_varies` (3982–3990)

These were previously inside `pipeline/mod.rs`'s `mod tests`; confirm whether they currently carry their own `#[cfg(feature = "auth-flow")]` — if the whole `mod tests` was gated they may not. Gating the new `mod tests` with `all(test, feature = "auth-flow")` is correct regardless: under `--no-default-features` these compile out (they test `auth-flow`-only fns).

- [ ] **Step 6: Run the FULL gate including both feature configs, then commit**

Pay attention to `cargo clippy --all-targets --no-default-features -- -D warnings` and `cargo test --no-default-features` — `auth.rs` must compile cleanly with the OAuth items absent.
```bash
git add -A
git commit -m "refactor(pipeline): extract auth module (token persist + gated OAuth bootstrap)"
```

---

### Task 8: Convert `telegram.rs` to a directory + extract `telegram/prompts.rs` (with fold-ins)

**Files:**
- Rename: `src/telegram.rs` → `src/telegram/mod.rs`
- Create: `src/telegram/prompts.rs`
- Modify: `src/telegram/mod.rs` (add `pub mod prompts;`; group_key fold-in comment)
- Modify: `src/pipeline/mod.rs` (call sites + `use`; remove moved items)

- [ ] **Step 1: Convert the file to a module directory**

```bash
mkdir src/telegram
git mv src/telegram.rs src/telegram/mod.rs
```
No `main.rs` change needed (`mod telegram;` still resolves to the directory).

- [ ] **Step 2: Declare the submodule and add the group_key collision fold-in**

In `src/telegram/mod.rs` add near the top (after existing `use`s):
```rust
pub mod prompts;
```
Then extend the `group_key` doc comment (currently ending at line 82, just before `#[must_use]`). Append this paragraph to that doc block:
```rust
/// FNV-1a is not collision-free: two distinct `raw_merchant` strings could (with
/// ~1/2^64 probability) hash to the same key and thus share one prompt — a reply
/// would then resolve both merchants together. At personal-finance volumes this
/// is astronomically unlikely and acceptable; revisit (e.g. salt + length-prefix,
/// or store the raw merchant alongside the key) only if collisions ever surface.
```

- [ ] **Step 3: Create `telegram/prompts.rs` and move the three prompt functions**

Create `src/telegram/prompts.rs`:
```rust
use std::collections::BTreeMap;

use tracing::warn;

use crate::config::PAYEE_THRESHOLD;
use crate::matching::best_payee_match;
use crate::state::{PendingReview, State};

use super::{ConfirmChannel, Prompt, encode_callback, group_key};
```
Move verbatim from `pipeline/mod.rs` (current `run.rs` lines):
| Item | Lines (doc → end) | Visibility | Note |
|------|-------|-----------|------|
| `fn build_prompt` (doc from 315; `fn` at 326) | 315–412 | `private → pub` | change its inner `use crate::telegram::{encode_callback, group_key};` (line 332) to `use super::{encode_callback, group_key};` |
| `async fn rehold_failed_remaps` (doc from 414; `fn` at 419) | 414–444 | `private → pub` | |
| `async fn send_group_prompts` (doc from 446; `fn` at 450) | 446–501 | `private → pub` | add the rate-limit fold-in (Step 4) |

- [ ] **Step 4: Add the Telegram rate-limit fold-in note**

In the moved `send_group_prompts`, add this comment immediately above the `for merchant in order {` loop:
```rust
    // Telegram rate limits: ~30 messages/sec globally and bursts to a single
    // chat are throttled (~1 msg/sec sustained). We send one prompt per held
    // merchant sequentially with no backoff; a run holding dozens of new
    // merchants at once could see a 429 (the send just warns and retries next
    // run — see the Err arm below). Acceptable at current volume; add pacing
    // here if held-merchant counts ever grow large.
```

- [ ] **Step 5: Repoint the call sites in `pipeline/mod.rs`**

`run()` calls `rehold_failed_remaps(...)` (line 866) and `send_group_prompts(...)` (line 1309); `send_group_prompts` calls `build_prompt` internally (moves with it). Add to `pipeline/mod.rs` `use`:
```rust
use crate::telegram::prompts::{rehold_failed_remaps, send_group_prompts};
```
(`build_prompt` is now private to `prompts.rs`'s callers — only `send_group_prompts` uses it, so `mod.rs` does not import it.)

- [ ] **Step 6: Move the prompt tests into `telegram/prompts.rs`**

Append `#[cfg(test)] mod tests { use super::*; use crate::pipeline::testutil::*; }` and move verbatim:
- `build_prompt_picks_flavor_by_payee_membership` (1747–1837)
- `build_prompt_keeps_buttons_regardless_of_external_id_length` (1839–1866)
- `build_prompt_category_flavor_when_payee_settled` (1868–1905)
- `build_prompt_payee_first_when_payee_unsettled` (1907–1934)
- `build_prompt_shows_txn_count_when_grouped` (1936–1967)
- `send_collapses_same_merchant_into_one_prompt` (2004–2038)
- `send_distinct_merchants_sends_two_prompts` (2040–2060)
- `send_adopts_existing_prompt_without_resending` (2062–2085)

> Leave the `failed_remap_*` tests (3577, 3636, 4316) and `awaiting_count_is_per_merchant` (1988) in `pipeline/mod.rs` — they drive `run()` end-to-end (integration), not `build_prompt` in isolation.

- [ ] **Step 7: Run the gate, then commit**

```bash
git add -A
git commit -m "refactor(telegram): extract prompts module; note group_key collisions + rate limits"
```

---

### Task 9: Extract `report.rs` (dry-run report + summary body)

**Files:**
- Create: `src/report.rs`
- Modify: `src/main.rs` (add `mod report;`)
- Modify: `src/pipeline/mod.rs` (call into `report::*`; remove the inlined println/body blocks)

- [ ] **Step 1: Declare the top-level module**

Add `mod report;` to `src/main.rs` (alongside the other `mod` lines).

- [ ] **Step 2: Create `report.rs` with the two functions**

Create `src/report.rs`:
```rust
use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::actual::shim_protocol::ImportTxn;
use crate::pipeline::HeldClassification;

/// Counts the dry-run report prints alongside the per-account row breakdown.
pub struct DryRunCounts {
    pub would_skip: usize,
    pub held: usize,
    pub transfers_imported: usize,
    pub transfers_held: usize,
    pub flex_mirrors_suppressed: usize,
}

/// Print the dry-run report. Pure output; no writes. The grouped import rows,
/// held classifications, and newly-cached pairs are exactly what a real run
/// WOULD persist.
pub fn dry_run(
    grouped: &BTreeMap<String, Vec<ImportTxn>>,
    held_classifications: &[HeldClassification],
    newly_cached: &[(String, String)],
    counts: &DryRunCounts,
) {
    // BODY: move verbatim from pipeline/mod.rs lines 1245–1283 (the println!
    // block), with these substitutions:
    //   - `grouped` is the &-param (was the owned local)
    //   - `outcome.held`            -> `held_classifications`
    //   - `newly_cached`            -> `newly_cached` (param)
    //   - `fetched_count.saturating_sub(candidates.len())` -> `counts.would_skip`
    //   - `held`                    -> `counts.held`
    //   - `transfers_imported`      -> `counts.transfers_imported`
    //   - `state.held_transfers.len()` -> `counts.transfers_held`
    //   - `flex_mirrors_suppressed` -> `counts.flex_mirrors_suppressed`
    // The leading `println!("DRY RUN — no writes to Actual");` and the `total`
    // computed as `grouped.values().map(Vec::len).sum()` stay as-is.
}

/// Build the Apprise notification body for a completed (non-dry) run.
#[must_use]
pub fn summary_body(
    added: usize,
    updated: usize,
    held: usize,
    newly_cached: &[(String, String)],
    awaiting: usize,
    transfers_imported: usize,
    held_transfers: usize,
) -> String {
    // BODY: move verbatim from pipeline/mod.rs lines 1332–1354, with:
    //   - the initial `let mut body = format!("Added {}, updated {} ...")`
    //     using the `added`/`updated`/`held` params
    //   - `state.held_transfers.len()` -> `held_transfers`
    //   - keep the `write!(body, ...)` calls; return `body`
}
```

- [ ] **Step 3: Replace the dry-run block in `run()`**

In `pipeline/mod.rs`, replace lines 1244–1287 (the `if dry_run { ... persist_rotated_token...; return Ok(()); }` block) with:
```rust
    if dry_run {
        report::dry_run(
            &grouped,
            &outcome.held,
            &newly_cached,
            &report::DryRunCounts {
                would_skip: fetched_count.saturating_sub(candidates.len()),
                held,
                transfers_imported,
                transfers_held: state.held_transfers.len(),
                flex_mirrors_suppressed,
            },
        );
        // Re-persist only the rotated token; discard the in-memory cache/pending/
        // last_run changes a dry-run must not commit.
        persist_rotated_token(cfg, state)?;
        return Ok(());
    }
```

- [ ] **Step 4: Replace the summary-body block in `run()`**

In `pipeline/mod.rs`, replace the body-building block (lines 1332–1354, from `let mut body = format!(...)` through the final `if !state.held_transfers.is_empty() { ... }`) with:
```rust
    let body = report::summary_body(
        summary.added,
        summary.updated,
        held,
        &newly_cached,
        awaiting,
        transfers_imported,
        state.held_transfers.len(),
    );
```
Leave the surrounding `info!(... "run complete")` structured log (1320–1331) and the `notifier.notify("Actual import complete", &body).await.ok();` (1355) in `run()`.

- [ ] **Step 5: Add `crate::report` import if needed + gate, then commit**

Reference `report::` fully-qualified (already crate-rooted as `report::` from within `pipeline`? No — `report` is a top-level module, so from `pipeline/mod.rs` use `crate::report`). Use `crate::report::dry_run` / `crate::report::summary_body` / `crate::report::DryRunCounts`, or add `use crate::report;` to `pipeline/mod.rs` and call `report::*`.

Run the gate. The dry-run integration tests (`dry_run_sends_no_prompts_and_no_writes`, `dry_run_persists_only_rotated_token`) and every summary path are the safety net here.
```bash
git add -A
git commit -m "refactor(report): extract dry-run report + summary body from run()"
```

---

### Task 10: Extract the transfer phases from `run()` into `transfer.rs` (aggressive)

**Files:**
- Modify: `src/pipeline/transfer.rs` (add `reconcile_flex_mirrors`, `resolve_intents`)
- Modify: `src/pipeline/mod.rs` (replace the two in-line blocks with calls)

- [ ] **Step 1: Add `reconcile_flex_mirrors` to `transfer.rs`**

Add these imports to `transfer.rs`:
```rust
use std::collections::BTreeSet;

use crate::banks::RawTransaction;
use crate::state::{HeldTransfer, State};
```
Add the function:
```rust
/// Reconcile this run's Flex mirror lines against main-side Flex transfers.
///
/// Splits `monzo_flex` (`FlexMirror`) rows out of `transfer_rows`, suppresses any
/// whose main-side twin (abs amount + date) is present THIS run, and (re)enqueues
/// unpaired mirrors onto `state.held_transfers` (dedup by external_id) so a mirror
/// held in an earlier run resolves once its twin arrives. Returns the remaining
/// non-mirror transfer rows and the count of suppressed mirrors.
pub fn reconcile_flex_mirrors(
    state: &mut State,
    transfer_rows: Vec<RawTransaction>,
) -> (Vec<RawTransaction>, usize) {
    // BODY: move verbatim from pipeline/mod.rs lines 792–838, where:
    //   - `transfer_rows` is the owned param (lines 803, 813 already operate on it)
    //   - after the partition at 813, the `transfer_rows` rebinding holds the
    //     non-mirror rows
    //   - replace the final implicit fall-through with:
    //         (transfer_rows, flex_mirrors_suppressed)
    // `flex_account_id`, `main_flex_keys`, `mirror_rows`, `prior_held_mirrors`,
    // and `flex_mirrors_suppressed` are all locals created inside this fn.
}
```

- [ ] **Step 2: Add `resolve_intents` to `transfer.rs`**

Add imports:
```rust
use crate::actual::shim_protocol::ImportTxn;
```
(`Payee`, `AccountLink`, `PotLink`, `TransferTarget`, `HeldTransfer` already imported above/Task 4.) Add:
```rust
/// Resolve transfer intents (the retry queue in `state.held_transfers` + this
/// run's freshly-fetched transfers, deduped by external_id) to Actual import
/// rows. Each resolved transfer becomes ONE uncategorised row in the SOURCE
/// account's group (Actual auto-creates the linked counterpart). Intents whose
/// source and/or destination is still unmapped are written back to
/// `state.held_transfers` for next-run retry. Returns (rows keyed by source
/// Actual account id, count imported).
pub fn resolve_intents(
    state: &mut State,
    transfer_rows: &[RawTransaction],
    account_links: &[AccountLink],
    pot_links: &[PotLink],
    payees: &[Payee],
) -> (Vec<(String, ImportTxn)>, usize) {
    // BODY: move verbatim from pipeline/mod.rs lines 914–964, where:
    //   - `let mut transfer_intents = state.held_transfers.clone();` ... (the
    //     work-list build at 918–927) stays
    //   - introduce `let mut rows: Vec<(String, ImportTxn)> = Vec::new();`
    //   - the success arm that did
    //         `grouped.entry(src_actual).or_default().push(ImportTxn { ... });`
    //     becomes
    //         `rows.push((src_actual, ImportTxn { ... }));`
    //   - `state.held_transfers = still_held;` stays
    //   - return `(rows, transfers_imported)`
    // `resolve_transfer_dest`, `actual_id_for`, `transfer_payee_id` are now
    // siblings in this module — call them unqualified.
}
```

- [ ] **Step 3: Replace the two in-line blocks in `run()`**

In `pipeline/mod.rs`, replace the Flex-mirror block (lines 787–838) with:
```rust
    let (transfer_rows, flex_mirrors_suppressed) =
        transfer::reconcile_flex_mirrors(state, transfer_rows);
```
(`transfer_rows` was bound at 783 from the first `partition`; this rebinds it to the non-mirror remainder. `flex_mirrors_suppressed` is consumed later by the dry-run report and `summary_body`.)

Then replace the transfer-resolution block (lines 909–964, from the `let pot_links = ...` work-list through `state.held_transfers = still_held;`) with:
```rust
    let account_links = state.monzo.accounts.clone();
    let pot_links = state.monzo.pots.clone();
    let (xfer_rows, transfers_imported) =
        transfer::resolve_intents(state, &transfer_rows, &account_links, &pot_links, &payees);
```
**Important ordering:** `account_links` was previously cloned at line 903 *before* the transfer loop; keep that single clone here (the assemble step in Task 11 reuses `account_links`). Remove the now-duplicate `let account_links = state.monzo.accounts.clone();` at 903 and the `let pot_links = state.monzo.pots.clone();` at 913 — they are folded into the snippet above.

The `grouped` map is still built in `run()`. Immediately after the call, seed `grouped` with the transfer rows FIRST (preserving today's transfers-before-imports ordering within each account Vec):
```rust
    let mut grouped: BTreeMap<String, Vec<ImportTxn>> = BTreeMap::new();
    let mut held = 0usize;
    for (aid, row) in xfer_rows {
        grouped.entry(aid).or_default().push(row);
    }
```
(This replaces the old `let mut grouped ...; let mut held = 0usize;` at lines 906–907. The `make_row`/`resolve_cat` closures and the cached/matched/resolved/held loops below are extracted in Task 11; until then they remain and still compile against `grouped`/`held`.)

- [ ] **Step 4: Run the gate, then commit**

The Flex tests (`flex_repayment_imports_main_side_and_suppresses_mirror`, `unpaired_flex_mirror_is_held_not_dropped`, `held_flex_mirror_suppressed_when_twin_arrives_next_run`, `flex_flagged_but_unmapped_holds_main_and_suppresses_mirror`, `direct_flex_purchase_imports_as_ordinary_spending_on_flex_account`) and the transfer tests (`pot_deposit_*`, `held_transfer_*`) are the safety net.
```bash
git add -A
git commit -m "refactor(pipeline): extract flex reconciliation + transfer resolution from run()"
```

---

### Task 11: Extract import-row assembly from `run()` into `import.rs` (aggressive)

**Files:**
- Modify: `src/pipeline/import.rs` (add `AssembleInputs`, `Assembled`, `assemble`)
- Modify: `src/pipeline/mod.rs` (replace the assembly block with one call)

- [ ] **Step 1: Define the I/O structs + `assemble` in `import.rs`**

Add imports to `import.rs`:
```rust
use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use tracing::warn;

use crate::actual::ActualClient;
use crate::actual::shim_protocol::{Category, ImportTxn, Payee};
use crate::state::ClassifiedHold;

use super::classify::HeldClassification;
use super::transfer::actual_id_for;
use super::classify::{category_id_for, payee_id_for};
```
Add the structs + function:
```rust
/// Read-only inputs to import-row assembly. Borrowed to avoid cloning the
/// candidate set + Actual catalog.
pub struct AssembleInputs<'a> {
    /// All non-scheduled candidates this run (used to resolve external_id -> tx).
    pub candidates: &'a [RawTransaction],
    /// FULL cache hits: (external_id, payee_name, category_name). Empty category = uncategorised.
    pub cached: &'a [(String, String, String)],
    /// Claude matches confident in BOTH dimensions: (external_id, payee_name, category_name).
    pub matched: &'a [(String, String, String)],
    /// Held classifications (≥1 uncertain dimension) to persist as pending reviews.
    pub held_classifications: &'a [HeldClassification],
    pub payees: &'a [Payee],
    pub categories: &'a [Category],
    pub account_links: &'a [crate::state::AccountLink],
    /// Dry-run suppresses payee creation on the resolved path.
    pub dry_run: bool,
}

/// What assembly produced: import rows (keyed by Actual account id), the count of
/// held items, and merchant->payee pairs newly folded into the cache (for the
/// summary/notification).
pub struct Assembled {
    pub grouped: BTreeMap<String, Vec<ImportTxn>>,
    pub held: usize,
    pub newly_cached: Vec<(String, String)>,
}

/// Assemble import rows from cached hits, fresh Claude matches, previously
/// resolved pending reviews, and newly held classifications. Mutates `state`
/// (payee/category caches, pending reviews) and may call `actual.create_payee`
/// for resolved `create_new` reviews. Fail-loud throughout: anything that no
/// longer resolves is re-held, never silently dropped or mis-imported.
pub async fn assemble(
    inputs: AssembleInputs<'_>,
    state: &mut State,
    actual: &ActualClient,
) -> Result<Assembled> {
    let mut grouped: BTreeMap<String, Vec<ImportTxn>> = BTreeMap::new();
    let mut held = 0usize;
    let mut newly_cached: Vec<(String, String)> = Vec::new();

    // Apply already-resolved pending reviews from previous runs. Taken here (not
    // in run()) so the resolved loop below and the held loop share one ordering.
    let resolved = state.take_resolved();

    // BODY: move verbatim from pipeline/mod.rs the assembly section, with the
    // substitutions listed in Step 2:
    //   - the `make_row` closure          (lines 966–972)
    //   - the `resolve_cat` closure        (lines 974–983)
    //   - the `by_id` map                  (lines 985–988)
    //   - cached-hits loop                 (lines 990–1067)
    //   - claude-matches loop              (lines 1068–1138)
    //   - resolved-reviews loop            (lines 1139–1218)
    //   - held-classifications loop        (lines 1220–1241)

    Ok(Assembled {
        grouped,
        held,
        newly_cached,
    })
}
```

- [ ] **Step 2: Apply these substitutions inside the moved body**

- `plan.cached`            → `inputs.cached`
- `outcome.matched`        → `inputs.matched`
- `outcome.held`           → `inputs.held_classifications`
- `candidates`             → `inputs.candidates` (in the `by_id` build only)
- `payees`                 → `inputs.payees`
- `categories`             → `inputs.categories`
- `account_links`          → `inputs.account_links`
- `dry_run`                → `inputs.dry_run`  (the `None if r.create_new && !dry_run` arm at line 1143)
- `actual.create_payee(...)` stays (the `actual` param)
- `actual_id_for`, `category_id_for`, `payee_id_for`, `hold_unmapped` are siblings/imports — call unqualified (`hold_unmapped` is already in this module; `actual_id_for` from `super::transfer`; `category_id_for`/`payee_id_for` from `super::classify`)
- `grouped`, `held`, `newly_cached` are the locals declared at the top of `assemble`
- `state.*` calls (`add_pending`, `add_pending_classified`, `add_pending_full`, `payee_cache`, `category_cache`) use the `state` param unchanged

- [ ] **Step 3: Replace the assembly block in `run()` with one call**

In `pipeline/mod.rs`, the block that previously held the closures + four loops (lines 966–1241) is now gone. Also delete the standalone `let resolved = state.take_resolved();` at line 869 (moved into `assemble`). After the transfer seeding from Task 10, insert:
```rust
    let assembled = import::assemble(
        import::AssembleInputs {
            candidates: &candidates,
            cached: &plan.cached,
            matched: &outcome.matched,
            held_classifications: &outcome.held,
            payees: &payees,
            categories: &categories,
            account_links: &account_links,
            dry_run,
        },
        state,
        actual,
    )
    .await?;

    // Merge assembled import rows into the transfer-seeded `grouped` (transfers
    // stay first within each account Vec — same order as before the split).
    for (aid, mut rows) in assembled.grouped {
        grouped.entry(aid).or_default().append(&mut rows);
    }
    let held = assembled.held;
    let newly_cached = assembled.newly_cached;
```
Notes:
- `candidates` is borrowed by `AssembleInputs` and also read later (dry-run `would_skip` uses `candidates.len()`); borrow ends at the `.await?`, so later reads are fine.
- `outcome.held` is borrowed for `held_classifications` and ALSO read later by `report::dry_run(&grouped, &outcome.held, ...)` — that read is after `assemble` returns, so no conflict.
- `held` here shadows nothing problematic: the old `let mut held = 0usize;` was moved into `assemble` (Task 10 Step 3 kept a transfer-only seed without a `held` counter — remove that stray `let mut held = 0usize;` if it was added, since `held` now comes from `assembled`).

> Reconcile with Task 10 Step 3: that step seeded `grouped` and declared `let mut held = 0usize;`. With Task 11, `held` is provided by `assembled`, so the Task-10 `let mut held = 0usize;` line is removed; only the `grouped` seed (transfer rows) and its `let mut grouped` declaration remain before the `assemble` call.

- [ ] **Step 4: Confirm `run()` is now a thin orchestrator**

`run()` should read top-to-bottom as: auth → fetch+partition → `reconcile_flex_mirrors` → read Actual payees/schedules/categories → drop scheduled → `rehold_failed_remaps` → `plan_resolution`/Claude/`apply_matches` → `resolve_intents` + seed grouped → `assemble` + merge → `if dry_run { report::dry_run; persist; return }` → import loop → `send_group_prompts` → `info!` summary + `report::summary_body` + notify. Target ≈120 lines. Keep the existing `#[allow(clippy::too_many_arguments, clippy::too_many_lines, clippy::future_not_send)]` on `run()` — the argument count is unchanged and the body, while shorter, may still trip `too_many_lines` thresholds; the gate decides whether any allow can be dropped (drop only what clippy confirms is now unnecessary).

- [ ] **Step 5: Run the FULL gate, then commit**

Every `run()` integration test is the safety net for this step — they must all stay green:
`medium_match_held_prompted_then_imported_after_confirm`, `imports_route_to_per_source_actual_account`, `high_confidence_imports_with_category`, `uncertain_category_holds_then_imports_after_category_confirm`, `stale_cached_category_reprompts_category_not_payee`, `high_confidence_match_to_nonexistent_payee_is_held_not_dropped`, `resolved_review_from_unmapped_account_is_reheld_not_dropped`, `create_payee_import_failure_leaves_review_for_retry`, `unconfirmed_hold_is_not_reprompted`, and the dry-run/auth/transfer/flex suites.
```bash
git add -A
git commit -m "refactor(pipeline): extract import-row assembly from run() into import::assemble"
```

---

## Final acceptance (after Task 11)

- [ ] `cargo fmt --all -- --check` clean
- [ ] `cargo clippy --all-targets -- -D warnings` clean — default features AND `--no-default-features`
- [ ] `cargo test` AND `cargo test --no-default-features` — same test *count* as HEAD (`2582ce9`), all green (no test was deleted; tests only relocated). Confirm the totals match: capture `cargo test 2>&1 | rg 'test result:'` before Task 1 and after Task 11.
- [ ] `src/pipeline/mod.rs` `run()` is ≈120 lines; no single file in `src/pipeline/` exceeds ~700 lines.
- [ ] `git log --oneline` shows one commit per task (11 commits).
- [ ] `rg 'crate::run' src/` returns nothing (all references migrated to `crate::pipeline`).
- [ ] Behaviour unchanged: the only non-mechanical diffs are the two fold-in comments (group_key collision note in `telegram/mod.rs`; rate-limit note in `telegram/prompts.rs`).

**Next phase:** Phase C — ID newtypes (`MonzoAccountId`, `MonzoPotId`, `ActualAccountId`, `ExternalTxId`), authored against this new module layout. Phase D (poll-side prompt chaining) lands last, in the new `telegram/prompts.rs`.
```
