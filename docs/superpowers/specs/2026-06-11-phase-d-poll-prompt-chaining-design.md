# Phase D — Poll-side Prompt Chaining: Design

**Date:** 2026-06-11
**Branch:** `refactor/phase-d-poll-prompt-chaining` (off merged `main` @ `f4975ec`)
**Status:** Approved design — ready for implementation plan
**Predecessors:** Phases A, B, C (all merged to local `main`). See `docs/superpowers/plans/2026-06-10-review-remediation-phase-{a,b,c}.md`.

## Problem

The payee → category resolution currently takes 2–3 `aba run` cycles to fully
settle a held transaction.

`run()` is the **only** place that sends Telegram prompts, because
`telegram::prompts::build_prompt` needs Actual's live **name catalogs**
(`payee_names: &[String]`, `category_names: &[String]`) to choose a prompt
flavor. `poll()` is deliberately firewalled from Actual ("Never touches
Actual"), so when a payee-settling reply lands in `poll()`, it cannot build the
follow-up category prompt. Instead it nulls `telegram_message_id` for the
settled-payee / category-open members and defers the category prompt to the
**next** `run()`. That deferral is the latency.

## Solution (one sentence)

Cache the two Actual name catalogs in `State` during `run()`, then let `poll()`
send the category prompt immediately — scoped to exactly the groups it just
settled — using those cached catalogs, collapsing the resolution to a single
cycle.

## Design decisions (locked with user)

1. **Re-prompt UX = "send fresh + tidy old."** Poll sends a *new* category
   prompt message (not an in-place edit of the payee prompt), and additionally
   edits away the answered payee prompt's now-dead inline keyboard so a late tap
   can't feed a stale `cf:<group_key>` callback back through poll.
2. **Scope = "scoped to what it settled."** Poll sends category prompts ONLY for
   the groups it transitioned payee-settled / category-open *this tick* — it does
   NOT opportunistically re-prompt unrelated groups whose `telegram_message_id`
   happens to be `None` (e.g. from a prior run's failed send). Poll's contract
   stays tight: "drain replies + chain the immediate next prompt for what I just
   resolved."

## 1. The catalog cache (`src/state.rs`)

Add two additive fields to `State`:

```rust
/// Actual's live payee-name catalog, snapshotted at the last successful run.
/// Lets `poll` build a category prompt without touching Actual. Distinct from
/// `payee_cache` (a raw-merchant -> resolved-name resolution map).
#[serde(default)]
pub actual_payee_names: Vec<String>,

/// Actual's live category-name catalog, snapshotted at the last successful run.
/// See `actual_payee_names`.
#[serde(default)]
pub actual_category_names: Vec<String>,
```

- Both `#[serde(default)]` → **purely additive, no schema-version bump**; old
  state files load with empty vecs. `State::default()` initialises them empty,
  and the manual `Default` impl gains the two `Vec::new()` lines.
- **Naming rationale:** named `actual_*_names` to make provenance explicit (the
  Actual catalog) and to avoid confusion with the existing `payee_cache` /
  `category_cache`, which are raw-merchant → resolved-name *resolution* maps — a
  different concept entirely.

`run()` writes both right after it builds `payee_names` / `category_names` from
Actual (`pipeline/mod.rs`, ~line 130, after the `payee_names` `let`):

```rust
state.actual_payee_names = payee_names.clone();
state.actual_category_names = category_names.clone();
```

The existing `state.save(...)` calls later in `run()` persist them; no new save
is added.

### Staleness

The cache can lag live Actual by up to one run cycle (a payee/category added or
deleted in Actual between the last `run` and this `poll`). This is **safe and
intended**: import re-validates every resolution against live Actual, so a
category prompt built from a slightly stale catalog can at worst cause a re-hold,
never a mis-import. Accepting bounded staleness is precisely what lets `poll`
stay firewalled from Actual.

## 2. The sender, scoped (`src/telegram/prompts.rs`)

Refactor `send_group_prompts` to delegate to a private inner function carrying an
optional merchant restriction:

```rust
// Existing public entry point — UNCHANGED signature. run() and all existing
// tests keep calling this verbatim.
pub async fn send_group_prompts(
    state, channel, payee_names, category_names, state_path,
) {
    send_group_prompts_inner(state, channel, payee_names, category_names, state_path, None).await
}

// New public entry point for poll: send only for the given raw_merchants.
pub async fn send_chained_category_prompts(
    state, channel, payee_names, category_names, state_path,
    only: &BTreeSet<String>,
) {
    send_group_prompts_inner(state, channel, payee_names, category_names, state_path, Some(only)).await
}

// Private: the existing loop body, plus a skip when `restrict` is Some and the
// merchant isn't in it.
async fn send_group_prompts_inner(
    state, channel, payee_names, category_names, state_path,
    restrict: Option<&BTreeSet<String>>,
) { ... }
```

- `inner` holds today's `send_group_prompts` body verbatim, with one added
  guard at the top of the `for merchant in order` loop:
  `if restrict.is_some_and(|r| !r.contains(&merchant)) { continue; }`.
- The restriction key is `raw_merchant` — the same key `inner` already buckets
  on — so poll passes the set of transitioned members' `raw_merchant` strings.
- **Zero churn** to `run()`'s call site or the existing `prompts.rs` tests:
  `send_group_prompts`'s public signature is unchanged.

## 3. The poll change (`src/poll.rs`)

Replace the current `if any { if matches!(payee-settling) { clear message_id } }`
block. For groups where a payee-settling reply (`Confirm` / `CreateNew` /
`Remap`) just left the group payee-settled **and** category-open:

1. **Capture** each transitioned group's old `telegram_message_id` and
   `raw_merchant`.
2. **Tidy:** call `channel.edit_message_text(old_id, "✅ Payee set — now
   categorising…")` once per distinct old `message_id`, stripping the answered
   payee keyboard. Best-effort — `warn!` and continue on error (mirrors
   `rehold_failed_remaps`'s edit-away pattern).
3. **Clear** those members' `telegram_message_id` (so the sender won't "adopt"
   an existing id and will send fresh).
4. **Send:** clone the two cached catalogs out of `state`
   (`state.actual_payee_names.clone()`, `state.actual_category_names.clone()` —
   needed because `send_chained_category_prompts` takes `&mut state`), then call
   `send_chained_category_prompts(state, channel, &payees, &cats,
   &cfg.state_path, &transitioned_merchants)`.

### Ordering / borrow notes

- Catalog vecs are cloned out of `state` **before** the `&mut state` send call.
- The old `message_id`s must be captured **before** they are cleared in step 3.
- The reply-application loop already collects `members` before mutating, so the
  existing borrow discipline is preserved.

### Defensive guard

If `actual_category_names` is empty, skip steps 2–4 and fall back to today's
bare-clear behavior (defer to next run). This path is essentially unreachable —
for a category dimension to be *open*, a prior `run()` must have fetched
categories and thus populated the cache — but it is a cheap belt-and-suspenders
against a malformed/old state file.

### Invariant

`poll.rs`'s "Never touches Actual" invariant **stays true**: poll only talks to
Telegram (which it already does via `poll_replies`). The module doc comment is
updated to note poll now also chains category prompts via the cached catalogs.

## 4. Testing

New / updated tests:

- **poll chains the category prompt:** a payee `Confirm` on a category-open group
  →
  - `FakeChannel.edited` records the keyboard tidy on the old `message_id`,
  - `FakeChannel.sent` records a new category-flavor prompt,
  - the group's members adopt the new `message_id`,
  - no Actual client is constructed or called (poll's signature already excludes
    it — structurally guaranteed).
- **scoping:** a second group with `telegram_message_id: None` and payee
  *unsettled* (simulating a prior run's failed send) must receive **no** prompt
  from poll in the same tick — proves `restrict` excludes non-transitioned
  groups.
- **empty-cache guard:** a category-open group but empty `actual_category_names`
  → poll falls back to bare-clear (member `message_id` becomes `None`, nothing
  sent).
- **tidy is best-effort:** using `FakeChannel::edit_always_errors` (existing
  variant), a failing `edit_message_text` does not abort poll — the category
  prompt is still sent and replies are still applied.
- **run() populates the cache:** extend an existing `pipeline` integration test
  to assert `state.actual_payee_names` / `actual_category_names` are non-empty
  after a run that read a non-empty Actual catalog.
- existing `build_prompt` / `send_group_prompts` tests: **unchanged**.

`FakeChannel` already records both `sent: Vec<Prompt>` and
`edited: Vec<(i64, String)>` and ships an error-injecting variant
(`edit_always_errors`), so no test-double changes are required.

## Blast radius

| File | Change |
|------|--------|
| `src/state.rs` | +2 `Vec<String>` fields, `Default` impl lines |
| `src/pipeline/mod.rs` | cache write (2 lines) after `payee_names`/`category_names` are built; `send_group_prompts` call site **unchanged** (its public signature is preserved) |
| `src/telegram/prompts.rs` | `inner` refactor + new `send_chained_category_prompts` |
| `src/poll.rs` | chaining logic (tidy → clear → scoped send) + new tests |

- **No wire-format change.** The two cache fields are additive and `#[serde(default)]`.
- **No schema-version bump.**
- **No new dependency.**

## Non-goals (YAGNI)

- Poll does **not** take over sending initial prompts. `run()` still sends the
  first prompt for every newly held group (including category prompts for
  entries born payee-settled via a cache hit). Poll only *chains* the category
  prompt after a payee reply it just applied.
- Poll does **not** retry prompts a prior `run()` failed to send (explicitly
  excluded by the "scoped" decision).
- No "create category" flow — unchanged from Phase 2; a category suggestion not
  in the (cached) catalog still falls through to the free-remap / Skip prompt.
