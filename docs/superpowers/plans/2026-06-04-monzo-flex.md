# Monzo Flex Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Import Monzo Flex activity into Actual with Flex as its own account: main-side `monzo_flex` lines become transfers to the Flex account, the Flex-side mirror is suppressed (pair-else-hold), and direct Flex purchases (`scheme=mastercard` on the Flex feed) flow through the existing categorised path — no double-counting.

**Architecture:** Auto-detect the Flex account by `description` prefix `monzoflex_` (flagged `is_flex` on its `AccountLink`); exclude the `monzoflexbackingloan_` account (403s). At fetch, a `monzo_flex` line from a non-Flex account → `TransferTarget::Account(flex_id)` (reuses the merged transfer-payee import); from the Flex account (or when no Flex configured) → `TransferTarget::FlexMirror`. In `run()`, FlexMirror rows are suppressed when an equal-amount/same-date main-side transfer exists this run, else held (fail-loud).

**Tech Stack:** Rust (tokio, serde, anyhow, chrono, reqwest); Node `@actual-app/api` shim; TOML state.

**Spec:** `docs/superpowers/specs/2026-06-04-monzo-flex-design.md`

**Branch:** `feat/transfers` (even with `main` after the pots-slice fast-forward). Merge to `main` at the end (local-first; no push).

**Conventions (rust-harness-gotchas memory):** `cargo test` (NOT `--lib`); finalize every task with `cargo fmt --all` (Edit hook reorders imports) then `cargo clippy --all-targets -- -D warnings`; quote spaced paths; never `git clean -fd`. Re-confirm line anchors with Serena before editing — they drift.

**Current facts (post-pots-merge):**
- `src/banks/mod.rs`: `enum TransferTarget { Pot(String), Account(String) }` still carries `#[allow(dead_code)]` (Account is matched but never constructed yet).
- `src/banks/monzo.rs`: `MonzoTx` has `scheme: String` + `metadata: TxMetadata { pot_id }`; `impl MonzoTx { fn transfer_target(&self) -> Option<TransferTarget> }` handles only `uk_retail_pot`; `fetch_account` closure calls `t.transfer_target()`. `list_accounts()` returns `Vec<(String,String)>` = `(id, description)`, filtering `!closed`.
- `src/state.rs`: `AccountLink { monzo_id, monzo_name, actual_id, actual_name }`; `HeldTransfer { external_id, source_monzo_id, date, amount, target_kind, target_id, raw_merchant }` with `target()` ("pot"/"account"→variant) and `from_row()`.
- `src/run.rs`: `map_accounts` builds `links = suggest_links(&monzo_accounts, …)`; the transfer block partitions `transfer_rows` (`transfer.is_some()`) and processes a held+new work list (resolve via `resolve_transfer_dest`/`actual_id_for`/`transfer_payee_id`).

---

## Task F1: `AccountLink.is_flex`

**Files:** Modify `src/state.rs` (`AccountLink`); Test in `src/state.rs` tests.

- [ ] **Step 1: Failing test.** In `src/state.rs` `mod tests`:

```rust
#[test]
fn account_link_is_flex_round_trips_and_defaults_false() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.toml");
    let mut s = State::default();
    s.monzo.accounts.push(AccountLink {
        monzo_id: "acc_flex".into(),
        monzo_name: "monzoflex_0000".into(),
        actual_id: "act-flex".into(),
        actual_name: "Flex".into(),
        is_flex: true,
    });
    // A normal account constructed via ..Default::default() must default is_flex=false.
    let normal = AccountLink { monzo_id: "acc_cur".into(), ..Default::default() };
    assert!(!normal.is_flex);
    s.save(&path).unwrap();
    let loaded = State::load(&path).unwrap();
    assert!(loaded.monzo.accounts[0].is_flex);
}
```

- [ ] **Step 2: Run; verify FAIL.** `cargo test account_link_is_flex_round_trips` → FAIL (no field `is_flex`).

- [ ] **Step 3: Implement.** Add to `AccountLink` (after `actual_name`):

```rust
    /// True when this is the user's Monzo Flex account (auto-detected by the
    /// `monzoflex_` description prefix at `aba accounts` time; overridable).
    /// `monzo_flex` lines from a NON-flex account become transfers TO this
    /// account; lines from this account are the suppressible mirror side.
    #[serde(default)]
    pub is_flex: bool,
```

- [ ] **Step 4: Fix `AccountLink { … }` literals lacking `..Default::default()`.** `rg "AccountLink \{" src` — any literal that lists all fields explicitly (not using `..Default::default()`) needs `is_flex: false`. Most tests use `..Default::default()`; the round-trip test above sets it explicitly. Build will name any missed literal.

- [ ] **Step 5: Run.** `cargo test` → all green (baseline 127 + 1 = 128). `cargo fmt --all`; `cargo clippy --all-targets -- -D warnings`.

- [ ] **Step 6: Commit.**
```bash
git add src/state.rs
git commit -m "feat(state): AccountLink.is_flex flag"
```

---

## Task F2: Detect Flex account in `aba accounts`; exclude backing-loan

**Files:** Modify `src/banks/monzo.rs` (`list_accounts`); `src/run.rs` (`suggest_links` or `map_accounts`). Test in both.

- [ ] **Step 1: Failing test (exclusion).** In `src/banks/monzo.rs` tests, add a test that `list_accounts`'s filter logic drops backing-loan accounts. Since `list_accounts` does HTTP, test the pure predicate instead — extract one. Add a free fn near `list_accounts`:

```rust
/// Whether a Monzo account (by its `description`) should be offered for mapping.
/// Excludes the internal Flex backing-loan account, which 403s on /transactions
/// and would fail-loud the whole run if mapped.
fn is_mappable_account(description: &str) -> bool {
    !description.starts_with("monzoflexbackingloan_")
}
```
Test:
```rust
#[test]
fn backing_loan_account_is_not_mappable() {
    assert!(!is_mappable_account("monzoflexbackingloan_0000X"));
    assert!(is_mappable_account("monzoflex_0000X"));   // the real Flex account IS mappable
    assert!(is_mappable_account("user_0000X"));
}
```

- [ ] **Step 2: Run; verify FAIL.** `cargo test backing_loan_account_is_not_mappable` → FAIL (fn missing).

- [ ] **Step 3: Implement exclusion.** Add the fn above; in `list_accounts`, change the filter from `.filter(|a| !a.closed)` to `.filter(|a| !a.closed && is_mappable_account(&a.description))`.

- [ ] **Step 4: Failing test (is_flex detection).** In `src/run.rs` tests:

```rust
#[test]
fn map_accounts_flags_flex_account_by_description_prefix() {
    // suggest_links produces AccountLinks; flex flag is applied by a helper.
    let monzo = vec![
        ("acc_cur".to_string(), "user_0000".to_string()),
        ("acc_flex".to_string(), "monzoflex_0000".to_string()),
    ];
    let actual = vec![("u-cur".to_string(), "Current".to_string()),
                      ("u-flex".to_string(), "Flex".to_string())];
    let links = suggest_links_with_flex(&monzo, &actual, ACCOUNT_MATCH_THRESHOLD);
    let flex = links.iter().find(|l| l.monzo_id == "acc_flex").unwrap();
    assert!(flex.is_flex);
    let cur = links.iter().find(|l| l.monzo_id == "acc_cur").unwrap();
    assert!(!cur.is_flex);
}
```

- [ ] **Step 5: Run; verify FAIL.** `cargo test map_accounts_flags_flex_account_by_description_prefix` → FAIL.

- [ ] **Step 6: Implement.** Rather than change `suggest_links`'s signature, add a thin wrapper that post-processes its output to set `is_flex` from the Monzo `description` (the `monzo_name` field holds the description, since `list_accounts` returns `(id, description)` and `suggest_links` stores it as `monzo_name`):

```rust
/// `suggest_links` plus Flex auto-detection: flag any link whose Monzo
/// description (stored in `monzo_name`) starts with `monzoflex_`.
fn suggest_links_with_flex(
    monzo: &[(String, String)],
    actual: &[(String, String)],
    threshold: f64,
) -> Vec<AccountLink> {
    let mut links = suggest_links(monzo, actual, threshold);
    for l in &mut links {
        if l.monzo_name.starts_with("monzoflex_") {
            l.is_flex = true;
        }
    }
    links
}
```

In `map_accounts`, change `let links = suggest_links(&monzo_accounts, &actual_accounts, ACCOUNT_MATCH_THRESHOLD);` to call `suggest_links_with_flex(...)`. In the printed mapping loop, append a ` [FLEX]` marker when `l.is_flex` so the user sees the detection:

```rust
        let flex_tag = if l.is_flex { "  [FLEX]" } else { "" };
        // append {flex_tag} to each printed mapping line (both mapped + unmapped arms)
```

- [ ] **Step 7: Run.** `cargo test` → green (128 + 2 = 130). `cargo fmt --all`; `cargo clippy --all-targets -- -D warnings`.

- [ ] **Step 8: Commit.**
```bash
git add src/banks/monzo.rs src/run.rs
git commit -m "feat(accounts): exclude flex backing-loan; auto-flag is_flex by description"
```

---

## Task F3: `TransferTarget::FlexMirror` + `HeldTransfer` mapping

**Files:** Modify `src/banks/mod.rs` (`TransferTarget`); `src/state.rs` (`HeldTransfer::target`/`from_row`). Test in `src/state.rs`.

- [ ] **Step 1: Failing test.** In `src/state.rs` tests:

```rust
#[test]
fn held_transfer_flex_mirror_roundtrip() {
    use crate::banks::TransferTarget;
    // from_row for a FlexMirror row -> target_kind "flex_mirror", empty id
    let row = crate::banks::RawTransaction {
        external_id: "tx_fm".into(),
        date: "2026-06-02".parse().unwrap(),
        amount_minor: 82822,
        raw_merchant: "Flex".into(),
        source_monzo_id: "acc_flex".into(),
        transfer: Some(TransferTarget::FlexMirror),
    };
    let ht = crate::state::HeldTransfer::from_row(&row).unwrap();
    assert_eq!(ht.target_kind, "flex_mirror");
    assert_eq!(ht.target_id, "");
    assert_eq!(ht.target(), Some(TransferTarget::FlexMirror));
}
```

- [ ] **Step 2: Run; verify FAIL.** `cargo test held_transfer_flex_mirror_roundtrip` → FAIL (no `FlexMirror`).

- [ ] **Step 3: Implement enum.** In `src/banks/mod.rs`, add the variant and REMOVE the now-obsolete `#[allow(dead_code)]` (F4 constructs `Account` and `FlexMirror`; `Pot` already constructed):

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferTarget {
    /// Monzo pot id (`pot_…`), matched against `[[monzo.pots]]`.
    Pot(String),
    /// Destination Monzo account id (`acc_…`), matched against `[[monzo.accounts]]`.
    /// Used for the main-account side of a Monzo Flex transfer (id = the Flex account).
    Account(String),
    /// The Flex-account side of a `monzo_flex` transfer — a mirror with no unique
    /// data. Suppressed in `run()` when its main-side twin is present, else held.
    FlexMirror,
}
```
(If clippy reports `Account`/`FlexMirror` unconstructed after this task alone, keep a temporary `#[allow(dead_code)]` with comment "constructed in Task F4"; F4 removes it. Prefer to leave the allow OFF and accept that this task may be committed together with F4 if clippy blocks — but try without first.)

- [ ] **Step 4: Implement HeldTransfer mapping.** In `src/state.rs`, extend `HeldTransfer::target()` and `from_row()`:

`target()` match: add `"flex_mirror" => Some(crate::banks::TransferTarget::FlexMirror),`.

`from_row()` match on `tx.transfer.as_ref()?`: add
```rust
            crate::banks::TransferTarget::FlexMirror => ("flex_mirror", String::new()),
```
(the existing arms return `(&str, String)`; keep that shape).

- [ ] **Step 5: Run.** `cargo test held_transfer_flex_mirror_roundtrip` → PASS; `cargo test` → green (130 + 1 = 131). `cargo fmt --all`; `cargo clippy --all-targets -- -D warnings`.

- [ ] **Step 6: Commit.**
```bash
git add src/banks/mod.rs src/state.rs
git commit -m "feat(banks): TransferTarget::FlexMirror + HeldTransfer mapping"
```

---

## Task F4: Classify `monzo_flex` lines in the Monzo fetch

**Files:** Modify `src/banks/monzo.rs` (`MonzoTx` classifier + `fetch_account`). Test in `src/banks/monzo.rs`.

The classifier needs Flex context: the Flex account's `monzo_id` (if any) and which account is being fetched.

- [ ] **Step 1: Failing tests.** In `src/banks/monzo.rs` tests (helper to build a `MonzoTx` with a scheme; mirror existing tests):

```rust
#[test]
fn monzo_flex_from_main_account_targets_flex_account() {
    let tx = MonzoTx {
        id: "tx_r".into(), created: "2026-06-01T10:00:00Z".parse().unwrap(),
        amount: -82822, description: "Flex".into(), decline_reason: None, merchant: None,
        scheme: "monzo_flex".into(), metadata: TxMetadata { pot_id: None },
    };
    // fetched from the MAIN account ("acc_cur"); flex account is "acc_flex"
    assert_eq!(
        tx.transfer_target_flex("acc_cur", Some("acc_flex")),
        Some(TransferTarget::Account("acc_flex".into()))
    );
}

#[test]
fn monzo_flex_from_flex_account_is_mirror() {
    let tx = MonzoTx {
        id: "tx_r2".into(), created: "2026-06-01T10:00:00Z".parse().unwrap(),
        amount: 82822, description: "Flex".into(), decline_reason: None, merchant: None,
        scheme: "monzo_flex".into(), metadata: TxMetadata { pot_id: None },
    };
    // fetched FROM the flex account
    assert_eq!(tx.transfer_target_flex("acc_flex", Some("acc_flex")), Some(TransferTarget::FlexMirror));
}

#[test]
fn monzo_flex_without_flex_account_configured_is_mirror_held() {
    let tx = MonzoTx {
        id: "tx_r3".into(), created: "2026-06-01T10:00:00Z".parse().unwrap(),
        amount: -100, description: "Flex".into(), decline_reason: None, merchant: None,
        scheme: "monzo_flex".into(), metadata: TxMetadata { pot_id: None },
    };
    assert_eq!(tx.transfer_target_flex("acc_cur", None), Some(TransferTarget::FlexMirror));
}

#[test]
fn mastercard_on_flex_feed_is_not_a_transfer() {
    // direct Flex purchase: scheme mastercard -> ordinary path
    let tx = MonzoTx {
        id: "tx_dp".into(), created: "2026-06-01T10:00:00Z".parse().unwrap(),
        amount: -14997, description: "SHOP".into(), decline_reason: None,
        merchant: Some(Merchant { name: "Shop".into() }),
        scheme: "mastercard".into(), metadata: TxMetadata { pot_id: None },
    };
    assert_eq!(tx.transfer_target_flex("acc_flex", Some("acc_flex")), None);
}
```
(Confirm the exact `Merchant` field name — earlier `Merchant { name }`. Adjust if the struct differs.)

- [ ] **Step 2: Run; verify FAIL.** `cargo test transfer_target_flex monzo_flex_ mastercard_on_flex` → FAIL (method missing).

- [ ] **Step 3: Implement.** Replace the `impl MonzoTx` classifier so pots still work and Flex is added:

```rust
impl MonzoTx {
    /// Pots need no account context.
    fn transfer_target(&self) -> Option<TransferTarget> {
        if self.scheme == "uk_retail_pot"
            && let Some(pot_id) = self.metadata.pot_id.as_deref()
        {
            return Some(TransferTarget::Pot(pot_id.to_owned()));
        }
        None
    }

    /// Full classifier with Flex context. `fetched_account_id` is the account
    /// this row was fetched from; `flex_account_id` is the configured Flex
    /// account's monzo id (if any).
    ///  - `uk_retail_pot`        -> Pot
    ///  - `monzo_flex` from the flex account, or with no flex configured -> FlexMirror
    ///  - `monzo_flex` from another account -> Account(flex_account_id) (the transfer)
    ///  - anything else (incl. `mastercard` direct Flex purchases) -> None (ordinary)
    fn transfer_target_flex(
        &self,
        fetched_account_id: &str,
        flex_account_id: Option<&str>,
    ) -> Option<TransferTarget> {
        if let Some(t) = self.transfer_target() {
            return Some(t); // pots
        }
        if self.scheme == "monzo_flex" {
            return match flex_account_id {
                Some(fid) if fid != fetched_account_id => Some(TransferTarget::Account(fid.to_owned())),
                _ => Some(TransferTarget::FlexMirror), // fetched from flex acct, or no flex configured
            };
        }
        None
    }
}
```

- [ ] **Step 4: Wire into `fetch_account`.** Before the map closure, compute the Flex account id from `self.monzo.accounts`:

```rust
        let flex_account_id: Option<String> = self
            .monzo
            .accounts
            .iter()
            .find(|a| a.is_flex)
            .map(|a| a.monzo_id.clone());
```
Then in the closure replace `let transfer = t.transfer_target();` with:
```rust
                let transfer = t.transfer_target_flex(monzo_id, flex_account_id.as_deref());
```
(`monzo_id` is `fetch_account`'s param — the account being fetched.)

- [ ] **Step 5: Run.** `cargo test` → green (131 + 4 = 135). `cargo fmt --all`; `cargo clippy --all-targets -- -D warnings` (this removes the last `dead_code` concern on `Account`/`FlexMirror`; if F3 left an allow, remove it now).

- [ ] **Step 6: Commit.**
```bash
git add src/banks/monzo.rs
git commit -m "feat(monzo): classify monzo_flex lines (main->Account(flex), flex-side->FlexMirror)"
```

---

## Task F5: FlexMirror pair-else-hold suppression in `run()`

**Files:** Modify `src/run.rs` (the transfer block). Test in `src/run.rs`.

Pairing rule: a FlexMirror row (newly fetched OR previously held) is **suppressed** if a main-side Flex transfer — a `transfer == Account(flex_id)` row in THIS run's fetched transfer rows — has the same `(abs amount, date)`. Else it is **held** (fail-loud).

- [ ] **Step 1: Failing integration tests.** Mirror Task 7/8 harness (`FakeBank`, `fake_actual_transfers`, `cfg_at`, `run(...)`). The fake actual already exposes a transfer payee `p_xfer_cur` for `act-cur`; add one for the Flex account (`act-flex`) — either extend `fake_actual_transfers` or add `fake_actual_flex` returning `{id:'p_xfer_flex', transfer_acct:'act-flex'}`.

```rust
#[tokio::test]
async fn flex_repayment_imports_main_side_and_suppresses_mirror() {
    // accounts: acc_cur->act-cur (main), acc_flex->act-flex (is_flex=true)
    // bank returns BOTH sides of a repayment:
    //   main: ext "tx_main", source acc_cur, amount -82822, transfer Account("acc_flex")
    //   flex: ext "tx_mirror", source acc_flex, amount +82822, transfer FlexMirror
    // get_payees has transfer payee p_xfer_flex (transfer_acct act-flex).
    // After run(false): routed.log has EXACTLY ONE import — into act-cur with payee p_xfer_flex;
    //   the mirror produced no import; state.held_transfers empty.
}

#[tokio::test]
async fn unpaired_flex_mirror_is_held_not_dropped() {
    // bank returns ONLY a flex-side mirror (no main twin):
    //   flex: ext "tx_lonely", source acc_flex, amount +500, transfer FlexMirror
    // After run(false): no import; state.held_transfers has 1 entry target_kind "flex_mirror";
    //   pending_reviews empty.
}
```
(Use the existing pattern: build `RawTransaction` literals with the `transfer` field set; the fetch classifier is not exercised here — these tests inject already-classified rows via `FakeBank`, isolating run()'s pairing logic.)

- [ ] **Step 2: Run; verify FAIL.** → FAIL (mirror currently flows into the Account/Pot intent loop and would be mishandled).

- [ ] **Step 3: Implement.** In `run()`, after the existing `partition` that yields `transfer_rows`, split FlexMirror rows out and pair them. Insert BEFORE the existing held+new Account/Pot intent loop:

```rust
    // Flex mirror handling: a `monzo_flex` line on the Flex feed mirrors a
    // main-side transfer. Suppress it when its twin (same abs amount + date) is
    // a main-side Account(flex) transfer this run; otherwise hold it (fail-loud).
    let flex_account_id: Option<String> = state
        .monzo
        .accounts
        .iter()
        .find(|a| a.is_flex)
        .map(|a| a.monzo_id.clone());

    use std::collections::BTreeSet;
    // Keys of main-side Flex transfers this run: (abs amount, date string).
    let main_flex_keys: BTreeSet<(i64, String)> = transfer_rows
        .iter()
        .filter(|t| match (&t.transfer, &flex_account_id) {
            (Some(TransferTarget::Account(dest)), Some(fid)) => dest == fid,
            _ => false,
        })
        .map(|t| (t.amount_minor.abs(), t.date.format("%Y-%m-%d").to_string()))
        .collect();

    // Split mirrors out of transfer_rows (the rest go to the existing intent loop).
    let (mirror_rows, transfer_rows): (Vec<RawTransaction>, Vec<RawTransaction>) =
        transfer_rows
            .into_iter()
            .partition(|t| matches!(t.transfer, Some(TransferTarget::FlexMirror)));

    let mut flex_mirrors_suppressed = 0usize;
    // Held flex_mirror entries from prior runs are re-evaluated too.
    let prior_held_mirrors: Vec<HeldTransfer> = state
        .held_transfers
        .iter()
        .filter(|h| h.target_kind == "flex_mirror")
        .cloned()
        .collect();
    // Drop prior held mirrors from the queue; we re-decide their fate below.
    state.held_transfers.retain(|h| h.target_kind != "flex_mirror");

    // Decide each mirror (new rows + prior held): suppress if paired, else hold.
    let new_mirror_hts = mirror_rows.iter().filter_map(HeldTransfer::from_row);
    for ht in prior_held_mirrors.into_iter().chain(new_mirror_hts) {
        let key = (ht.amount.abs(), ht.date.clone());
        if main_flex_keys.contains(&key) {
            flex_mirrors_suppressed += 1; // paired -> drop (Actual recreates this side)
        } else {
            state.enqueue_held_transfer(ht); // unpaired -> hold (dedup by external_id)
        }
    }
```

Notes:
- `transfer_rows` is now shadowed to exclude mirrors; the existing Account/Pot intent loop below is unchanged and never sees FlexMirror (so `resolve_transfer_dest` is never called with it).
- `enqueue_held_transfer` (the previously-`#[allow(dead_code)]` helper) is now USED — remove its allow in `src/state.rs`.
- Place this block where `transfer_rows`, `state`, and `RawTransaction`/`TransferTarget`/`HeldTransfer` are in scope (right after the partition from Task 7). Ensure `TransferTarget` and `HeldTransfer` are imported in run.rs (they are).

Add to the dry-run output and the real-path summary a line when `flex_mirrors_suppressed > 0` and reflect held mirrors (the existing held-transfer summary line already counts `state.held_transfers`, which now includes held mirrors). For dry-run, add:
```rust
        println!("  flex mirrors suppressed: {flex_mirrors_suppressed}");
```

- [ ] **Step 4: Run.** `cargo test flex_repayment_imports_main_side_and_suppresses_mirror unpaired_flex_mirror_is_held_not_dropped` → PASS; then `cargo test` (135 + 2 = 137). `cargo fmt --all`; `cargo clippy --all-targets -- -D warnings`.

- [ ] **Step 5: Commit.**
```bash
git add src/run.rs src/state.rs
git commit -m "feat(run): FlexMirror pair-else-hold suppression"
```

---

## Task F6: Direct-purchase + end-to-end regression tests; RUNBOOK

**Files:** Test in `src/run.rs`; Modify `docs/RUNBOOK.md`.

- [ ] **Step 1: Failing/guard tests.**

```rust
#[tokio::test]
async fn direct_flex_purchase_imports_as_ordinary_spending_on_flex_account() {
    // acc_flex->act-flex (is_flex). Bank returns ONE row:
    //   ext "tx_dp", source acc_flex, amount -14997, transfer None (mastercard direct purchase),
    //   raw_merchant "Shop".
    // FakeClaude classifies "Shop" -> payee/category confidently (existing path).
    // After run(false): routed.log has ONE import into act-flex with the real payee
    //   (NOT a transfer payee) and a category; held_transfers empty; no transfer.
}

#[tokio::test]
async fn flex_it_moves_expense_via_transfer() {
    // Optional but valuable: main +5900 transfer Account(acc_flex) + flex -5900 FlexMirror.
    // Assert one transfer import into act-cur via p_xfer_flex; mirror suppressed.
    // (If redundant with the repayment test, keep just one; YAGNI.)
}
```

- [ ] **Step 2: Run; verify** (the direct-purchase test should pass given F4 leaves `mastercard` as `transfer:None`; it's a regression guard). Run the two names. If `direct_flex_purchase…` fails, diagnose — it must take the ordinary path. Iterate.

- [ ] **Step 3: RUNBOOK.** In `docs/RUNBOOK.md`, near the pots/accounts mapping docs, add a short Flex paragraph:
  - `aba accounts` auto-detects the Monzo Flex account (description `monzoflex_`, shown as `[FLEX]`) and flags it `is_flex` in `[[monzo.accounts]]`; the internal `monzoflexbackingloan_` account is excluded.
  - Map the Flex account to a dedicated Actual account (a credit/liability account).
  - Repayments and "flex it" moves import as transfers between your main and Flex accounts (one side imported, the mirror suppressed); direct Flex purchases import as ordinary categorised spending on the Flex account.
  - Any unexpected one-sided `monzo_flex` line is held for review, never dropped.

- [ ] **Step 4: Full gate.** `cargo test` (137 + tests added here), `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`.

- [ ] **Step 5: Commit.**
```bash
git add src/run.rs docs/RUNBOOK.md
git commit -m "test(run): direct Flex purchase + flex-it regression; document Flex in RUNBOOK"
```

---

## FLEX SLICE GATE

- [ ] Full gate green: `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`.
- [ ] Final whole-slice review (as for the pots slice) — focus: no double-count across the three scenarios; mirror suppression never drops unpaired/unique data; direct purchases unaffected; ordinary + pots paths unchanged.
- [ ] Merge to `main` (fast-forward, local-first; push only on explicit request).

## Self-Review (completed during authoring)

- **Spec coverage:** §1 detection → F1,F2; §2 classification → F4; §3 pair-else-hold → F5; §4 FlexMirror variant → F3; §5 direct purchase → F6. All covered.
- **Type consistency:** `AccountLink.is_flex: bool`; `TransferTarget::FlexMirror`; `HeldTransfer` target_kind `"flex_mirror"`/empty id; `transfer_target_flex(fetched_account_id, flex_account_id)`; `suggest_links_with_flex`; `is_mappable_account`. Names consistent across tasks.
- **Placeholders:** none — every code step shows code; integration tests defer mock-wiring to the established Task 7/8 harness pattern (`FakeBank`/`fake_actual_transfers`/`cfg_at`), which the implementer mirrors.
- **Fail-loud:** mirrors dropped ONLY when a twin is present this run; unpaired mirrors held (dedup by external_id), surfaced; backing-loan excluded so fetch never 403-fails.
