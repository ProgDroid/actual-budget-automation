# Phase 3 — Transfers (Pots + Inter-Account) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Import Monzo pot moves and Monzo→Monzo transfers as proper *linked Actual transfers* (via the destination account's transfer payee) instead of ordinary categorised spending, holding any transfer whose endpoint isn't mapped yet in a durable retry queue.

**Architecture:** A `RawTransaction` gains an `Option<TransferTarget>` discriminator parsed at fetch from Monzo `scheme`/`metadata`/`counterparty`. In `run`, transfers bypass Claude/payee/category classification: each resolves its target to an Actual account, looks up that account's transfer payee (`transfer_acct`), and imports a single uncategorised row whose `payee` is that transfer payee — Actual auto-creates the counterpart (`runTransfers` defaults true). Unmapped endpoints go to a durable `held_transfers` queue retried every run. Inter-account moves (double-sided in the feed) keep the outflow and suppress the inflow.

**Tech Stack:** Rust (tokio, serde, anyhow, chrono, reqwest), a Node `@actual-app/api` shim communicating via single-message JSON over stdio, TOML state file.

**Spec:** `docs/superpowers/specs/2026-06-04-transfers-design.md`

**Build order:** Tasks 1–9 are the **pots slice** — implement, get green (all tests + `cargo clippy --all-targets -- -D warnings` + `cargo fmt --all -- --check`), then merge to `main`. Tasks 10–13 are the **inter-account slice** on top.

**Conventions (from `rust-harness-gotchas` memory):**
- Tests: `cargo test` (NOT `--lib`). Finalize every task with `cargo fmt --all` (the Edit hook reorders imports; fmt makes it deterministic) and `cargo clippy --all-targets -- -D warnings`.
- Never `git clean -fd` (nukes untracked `.serena/`). Quote any path containing the home dir.
- Line numbers below are anchors from the current tree; re-confirm with `get_symbols_overview` before editing — they drift as you add code.

---

## File Structure

| File | Responsibility | Change |
|------|----------------|--------|
| `src/banks/mod.rs` | `RawTransaction`, `TransferTarget` | Add `TransferTarget` enum + `transfer` field |
| `src/banks/monzo.rs` | Monzo fetch + parse | Widen `MonzoTx`; classify pot/account transfers |
| `src/state.rs` | `State`, `MonzoState`, `PotLink`, `HeldTransfer` | Add `PotLink`, `pots`, `held_transfers`, enqueue helper |
| `src/actual/shim_protocol.rs` | wire types | Add `transfer_acct` to `Payee` |
| `actual-shim/index.js` | Node shim | `get_payees` returns `transfer_acct` |
| `src/run.rs` | orchestration + `aba accounts` | Transfer resolution helpers, retry queue, dedup, pot mapping, summary |
| `RUNBOOK.md` | ops docs | Pots mapping step |

---

# POTS SLICE (Tasks 1–9)

## Task 1: `TransferTarget` + `RawTransaction.transfer`

**Files:**
- Modify: `src/banks/mod.rs:9-21` (`RawTransaction`)
- Test: `src/banks/mod.rs` (`#[cfg(test)] mod tests`)

- [ ] **Step 1: Add the enum + field.** In `src/banks/mod.rs`, above `RawTransaction`, add:

```rust
/// Destination of a transfer detected in the bank feed. Resolved to an Actual
/// account (and thence its transfer payee) at import time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferTarget {
    /// Monzo pot id (`pot_…`), matched against `[[monzo.pots]]`.
    Pot(String),
    /// Destination Monzo account id (`acc_…`), matched against `[[monzo.accounts]]`.
    Account(String),
}
```

Then add this field to `RawTransaction` (after `source_monzo_id`):

```rust
    /// `Some` when this row is a transfer (pot move or inter-account); routes to
    /// the destination account's transfer payee instead of payee/category
    /// classification. `None` = ordinary transaction (Phase 1/2 path, unchanged).
    pub transfer: Option<TransferTarget>,
}
```

- [ ] **Step 2: Fix all `RawTransaction { … }` literals to compile.** Search and add `transfer: None` to every struct literal:

Run: `rg "RawTransaction \{" src --type rust -l`
Expected files include `src/banks/monzo.rs` (the `fetch_account` map closure) and any test builders in `src/run.rs`, `src/matching.rs`, `src/state.rs` tests. Add `transfer: None,` to each. (The monzo.rs production literal is replaced in Task 3, but add `transfer: None` now so the tree compiles.)

- [ ] **Step 3: Build.** Run: `cargo build`. Expected: PASS (no errors). If a literal was missed, the compiler names the file:line — fix it.

- [ ] **Step 4: Commit.**

```bash
git add src/
git commit -m "feat(banks): TransferTarget + RawTransaction.transfer discriminator"
```

---

## Task 2: `PotLink` + `MonzoState.pots`

**Files:**
- Modify: `src/state.rs:25-36` (`MonzoState`), and add `PotLink` near `AccountLink` (`src/state.rs:38-52`)
- Test: `src/state.rs` tests module

- [ ] **Step 1: Write the failing test.** In `src/state.rs` `mod tests`, add:

```rust
#[test]
fn pots_round_trip_through_state_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.toml");
    let mut s = State::default();
    s.monzo.pots.push(PotLink {
        monzo_pot_id: "pot_123".into(),
        monzo_pot_name: "Savings".into(),
        actual_id: "act-uuid".into(),
        actual_name: "Savings (Actual)".into(),
    });
    s.save(&path).unwrap();
    let loaded = State::load(&path).unwrap();
    assert_eq!(loaded.monzo.pots.len(), 1);
    assert_eq!(loaded.monzo.pots[0].monzo_pot_id, "pot_123");
    assert_eq!(loaded.monzo.pots[0].actual_id, "act-uuid");
}
```

- [ ] **Step 2: Run it; verify it fails.** Run: `cargo test pots_round_trip_through_state_file`. Expected: FAIL (`no field pots on MonzoState` / `cannot find PotLink`).

- [ ] **Step 3: Implement.** Add after `AccountLink` (around `src/state.rs:52`):

```rust
/// One Monzo pot mapped to one Actual account. Pot moves become Actual transfers
/// into this account. Empty `actual_id` = unmapped (its moves are held).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PotLink {
    /// Monzo pot id (`pot_…`) — matches `metadata.pot_id` in the feed and `GET /pots[].id`.
    pub monzo_pot_id: String,
    #[serde(default)]
    pub monzo_pot_name: String,
    /// Actual account UUID — the transfer target. Empty = unmapped.
    #[serde(default)]
    pub actual_id: String,
    #[serde(default)]
    pub actual_name: String,
}
```

Add to `MonzoState` (after `accounts`):

```rust
    /// Monzo pot → Actual account links. Empty until `aba accounts` populates it.
    #[serde(default)]
    pub pots: Vec<PotLink>,
}
```

- [ ] **Step 4: Run the test; verify it passes.** Run: `cargo test pots_round_trip_through_state_file`. Expected: PASS.

- [ ] **Step 5: Confirm `monzo_clone` carries pots.** Read `src/state.rs:237-244`. `monzo_clone` returns `self.monzo.clone()` (or rebuilds it). If it **rebuilds field-by-field**, add `pots: self.monzo.pots.clone(),`. Run: `cargo test`. Expected: PASS (whole suite still green).

- [ ] **Step 6: Commit.**

```bash
git add src/state.rs
git commit -m "feat(state): PotLink + MonzoState.pots"
```

---

## Task 3: Parse pot transfers in the Monzo feed

**Files:**
- Modify: `src/banks/monzo.rs:34-45` (`MonzoTx`); add a `TxMetadata` struct; `src/banks/monzo.rs:188-201` (the `fetch_account` map closure)
- Test: `src/banks/monzo.rs` tests module

- [ ] **Step 1: Write the failing test.** In `src/banks/monzo.rs` `mod tests`, add a unit test for a pure classifier we will extract:

```rust
#[test]
fn classifies_pot_deposit_as_transfer() {
    // A pot deposit: scheme uk_retail_pot, negative amount, metadata.pot_id set.
    let tx = MonzoTx {
        id: "tx_1".into(),
        created: "2026-06-01T10:00:00Z".parse().unwrap(),
        amount: -5000,
        description: "pot_…".into(),
        decline_reason: None,
        merchant: None,
        scheme: "uk_retail_pot".into(),
        metadata: TxMetadata { pot_id: Some("pot_abc".into()) },
    };
    assert_eq!(tx.transfer_target(), Some(TransferTarget::Pot("pot_abc".into())));
}

#[test]
fn ordinary_purchase_has_no_transfer_target() {
    let tx = MonzoTx {
        id: "tx_2".into(),
        created: "2026-06-01T10:00:00Z".parse().unwrap(),
        amount: -300,
        description: "Coffee".into(),
        decline_reason: None,
        merchant: None,
        scheme: "mastercard".into(),
        metadata: TxMetadata { pot_id: None },
    };
    assert_eq!(tx.transfer_target(), None);
}
```

(`use super::*;` already present in the tests module exposes `TransferTarget` via `crate::banks::TransferTarget` — add `use crate::banks::TransferTarget;` to the test module if not in scope.)

- [ ] **Step 2: Run it; verify it fails.** Run: `cargo test classifies_pot_deposit_as_transfer`. Expected: FAIL (`no field scheme/metadata`, `no method transfer_target`, `cannot find TxMetadata`).

- [ ] **Step 3: Implement.** Widen `MonzoTx` and add `TxMetadata` + the classifier. Replace `MonzoTx` (`src/banks/monzo.rs:34-45`) with:

```rust
#[derive(Debug, Deserialize)]
struct MonzoTx {
    id: String,
    created: DateTime<Utc>,
    amount: i64,
    #[serde(default)]
    description: String,
    #[serde(default)]
    decline_reason: Option<String>,
    #[serde(default)]
    merchant: Option<Merchant>,
    /// Monzo settlement scheme: `uk_retail_pot` (pot move), `p2p_payment`
    /// (Monzo→Monzo), `payport_faster_payments`, card schemes, etc.
    #[serde(default)]
    scheme: String,
    #[serde(default)]
    metadata: TxMetadata,
}

/// Subset of Monzo's `metadata` we use to classify transfers. Monzo sends many
/// more keys; serde ignores the rest.
#[derive(Debug, Default, Deserialize)]
struct TxMetadata {
    /// Present (a `pot_…` id) on pot deposits/withdrawals; matches `GET /pots[].id`.
    #[serde(default)]
    pot_id: Option<String>,
}

impl MonzoTx {
    /// Classify this row as a transfer, if it is one. Pots only in this slice;
    /// inter-account (`p2p_payment`) is added in the inter-account slice.
    fn transfer_target(&self) -> Option<crate::banks::TransferTarget> {
        if self.scheme == "uk_retail_pot" {
            if let Some(pot_id) = self.metadata.pot_id.clone() {
                return Some(crate::banks::TransferTarget::Pot(pot_id));
            }
        }
        None
    }
}
```

- [ ] **Step 4: Run the new tests; verify they pass.** Run: `cargo test transfer_target`. Wait — run by name: `cargo test classifies_pot_deposit_as_transfer` and `cargo test ordinary_purchase_has_no_transfer_target`. Expected: PASS.

- [ ] **Step 5: Wire the classifier into `fetch_account`.** In the map closure (`src/banks/monzo.rs:188-201`), set the field. The closure currently builds `RawTransaction { … source_monzo_id: monzo_id.to_string() }`. Capture the target before the move:

```rust
            .map(|t| {
                let raw_merchant = match &t.merchant {
                    Some(m) if !m.name.is_empty() => m.name.clone(),
                    _ => t.description.clone(),
                };
                let transfer = t.transfer_target();
                RawTransaction {
                    external_id: t.id,
                    date: t.created.date_naive(),
                    amount_minor: t.amount,
                    raw_merchant,
                    source_monzo_id: monzo_id.to_string(),
                    transfer,
                }
            })
```

(Remove the temporary `transfer: None` added in Task 1 here.)

- [ ] **Step 6: Run the whole suite.** Run: `cargo test`. Expected: PASS.

- [ ] **Step 7: Commit.**

```bash
git add src/banks/monzo.rs
git commit -m "feat(monzo): classify uk_retail_pot moves as Pot transfers"
```

---

## Task 4: `HeldTransfer` + durable retry queue on `State`

**Files:**
- Modify: `src/state.rs:8-23` (`State`); add `HeldTransfer` struct; add an enqueue method in `impl State`
- Test: `src/state.rs` tests module

- [ ] **Step 1: Write the failing test.**

```rust
#[test]
fn enqueue_held_transfer_dedupes_by_external_id() {
    let mut s = State::default();
    let mk = || HeldTransfer {
        external_id: "tx_1".into(),
        source_monzo_id: "acc_src".into(),
        date: "2026-06-01".into(),
        amount: -5000,
        target_kind: "pot".into(),
        target_id: "pot_abc".into(),
        raw_merchant: "Savings".into(),
    };
    assert!(s.enqueue_held_transfer(mk()));   // first insert -> true
    assert!(!s.enqueue_held_transfer(mk()));   // duplicate -> false, no growth
    assert_eq!(s.held_transfers.len(), 1);
}

#[test]
fn held_transfer_round_trips_through_state_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.toml");
    let mut s = State::default();
    s.enqueue_held_transfer(HeldTransfer {
        external_id: "tx_1".into(),
        source_monzo_id: "acc_src".into(),
        date: "2026-06-01".into(),
        amount: -5000,
        target_kind: "pot".into(),
        target_id: "pot_abc".into(),
        raw_merchant: "Savings".into(),
    });
    s.save(&path).unwrap();
    let loaded = State::load(&path).unwrap();
    assert_eq!(loaded.held_transfers.len(), 1);
    assert_eq!(loaded.held_transfers[0].target_id, "pot_abc");
}
```

- [ ] **Step 2: Run them; verify they fail.** Run: `cargo test held_transfer`. Expected: FAIL (`cannot find HeldTransfer`, `no field held_transfers`, `no method enqueue_held_transfer`).

- [ ] **Step 3: Implement.** Add the struct near `PendingReview` in `src/state.rs`:

```rust
/// A transfer whose source and/or destination account is not yet mapped. Held
/// durably and retried every run — never imported into the wrong place, never
/// dropped (fail-loud). `target_kind`/`target_id` are stored as plain strings
/// (not a `TransferTarget` enum) to keep the TOML state file clean.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldTransfer {
    pub external_id: String,
    pub source_monzo_id: String,
    pub date: String,
    pub amount: i64,
    /// "pot" or "account".
    pub target_kind: String,
    /// `pot_…` (pot) or `acc_…` (account).
    pub target_id: String,
    pub raw_merchant: String,
}

impl HeldTransfer {
    /// Reconstruct the typed target. Unknown `target_kind` → `None` (defensive).
    pub fn target(&self) -> Option<crate::banks::TransferTarget> {
        match self.target_kind.as_str() {
            "pot" => Some(crate::banks::TransferTarget::Pot(self.target_id.clone())),
            "account" => Some(crate::banks::TransferTarget::Account(self.target_id.clone())),
            _ => None,
        }
    }

    /// Build from a fetched transfer row.
    pub fn from_row(tx: &crate::banks::RawTransaction) -> Option<Self> {
        let (target_kind, target_id) = match tx.transfer.as_ref()? {
            crate::banks::TransferTarget::Pot(id) => ("pot", id.clone()),
            crate::banks::TransferTarget::Account(id) => ("account", id.clone()),
        };
        Some(Self {
            external_id: tx.external_id.clone(),
            source_monzo_id: tx.source_monzo_id.clone(),
            date: tx.date.format("%Y-%m-%d").to_string(),
            amount: tx.amount_minor,
            target_kind: target_kind.to_string(),
            target_id,
            raw_merchant: tx.raw_merchant.clone(),
        })
    }
}
```

Add to `State` (after `pending_reviews`):

```rust
    /// Transfers awaiting account/pot mapping; retried every run (see HeldTransfer).
    #[serde(default)]
    pub held_transfers: Vec<HeldTransfer>,
```

Add the method to `impl State`:

```rust
    /// Enqueue a held transfer if not already queued (dedup by `external_id`).
    /// Returns `true` if newly added.
    pub fn enqueue_held_transfer(&mut self, ht: HeldTransfer) -> bool {
        if self.held_transfers.iter().any(|h| h.external_id == ht.external_id) {
            return false;
        }
        self.held_transfers.push(ht);
        true
    }
```

- [ ] **Step 4: Run the tests; verify pass.** Run: `cargo test held_transfer`. Expected: PASS.

- [ ] **Step 5: Commit.**

```bash
git add src/state.rs
git commit -m "feat(state): HeldTransfer durable retry queue"
```

---

## Task 5: Shim exposes `transfer_acct` on payees

**Files:**
- Modify: `actual-shim/index.js:40-43` (`get_payees` case)
- Modify: `src/actual/shim_protocol.rs:41-44` (`Payee`)
- Test: `src/actual/mod.rs` tests (deserialization), and a manual shim check

- [ ] **Step 1: Write the failing test.** In `src/actual/mod.rs` `mod tests`, add a deserialization test proving `Payee` accepts `transfer_acct`:

```rust
#[test]
fn payee_deserializes_transfer_acct() {
    use crate::actual::shim_protocol::Payee;
    let json = r#"[
        {"id":"p1","name":"Tesco","transfer_acct":null},
        {"id":"p2","name":"Transfer: Savings","transfer_acct":"act-uuid"}
    ]"#;
    let payees: Vec<Payee> = serde_json::from_str(json).unwrap();
    assert_eq!(payees[0].transfer_acct, None);
    assert_eq!(payees[1].transfer_acct.as_deref(), Some("act-uuid"));
}
```

- [ ] **Step 2: Run it; verify it fails.** Run: `cargo test payee_deserializes_transfer_acct`. Expected: FAIL (`no field transfer_acct on Payee`).

- [ ] **Step 3: Implement (Rust).** Replace `Payee` (`src/actual/shim_protocol.rs:41-44`) with:

```rust
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Payee {
    pub id: String,
    pub name: String,
    /// Set (an account id) when this payee is a *transfer payee* — importing a
    /// transaction with this payee creates a linked transfer into that account.
    /// `None`/absent for ordinary payees.
    #[serde(default)]
    pub transfer_acct: Option<String>,
}
```

- [ ] **Step 4: Implement (shim).** Replace the `get_payees` case (`actual-shim/index.js:40-43`) with:

```js
    case 'get_payees': {
      const payees = await api.getPayees();
      return payees.map((p) => ({
        id: p.id,
        name: p.name,
        transfer_acct: p.transfer_acct ?? null,
      }));
    }
```

- [ ] **Step 5: Run the Rust test; verify pass.** Run: `cargo test payee_deserializes_transfer_acct`. Expected: PASS.

- [ ] **Step 6: Run the whole suite** (existing `get_payees` callers must still deserialize — `transfer_acct` defaults). Run: `cargo test`. Expected: PASS.

- [ ] **Step 7: Commit.**

```bash
git add src/actual/shim_protocol.rs actual-shim/index.js src/actual/mod.rs
git commit -m "feat(actual): expose transfer_acct on payees (shim + wire type)"
```

---

## Task 6: Transfer-resolution helpers (pure, unit-tested)

These pure functions hold the routing logic so `run` stays thin and the logic is testable without a live shim.

**Files:**
- Modify: `src/run.rs` (add helpers near `actual_id_for`, `src/run.rs:23-40`)
- Test: `src/run.rs` tests module

- [ ] **Step 1: Write the failing tests.** In `src/run.rs` `mod tests`:

```rust
#[test]
fn transfer_payee_id_finds_the_account_transfer_payee() {
    use crate::actual::shim_protocol::Payee;
    let payees = vec![
        Payee { id: "p1".into(), name: "Tesco".into(), transfer_acct: None },
        Payee { id: "p2".into(), name: "Transfer: Savings".into(), transfer_acct: Some("act-pot".into()) },
    ];
    assert_eq!(transfer_payee_id(&payees, "act-pot"), Some("p2"));
    assert_eq!(transfer_payee_id(&payees, "act-missing"), None);
}

#[test]
fn resolve_transfer_dest_maps_pot_then_account() {
    use crate::banks::TransferTarget;
    let pots = vec![PotLink {
        monzo_pot_id: "pot_abc".into(), monzo_pot_name: "Savings".into(),
        actual_id: "act-pot".into(), actual_name: "Savings".into(),
    }];
    let accts = vec![AccountLink {
        monzo_id: "acc_joint".into(), monzo_name: "Joint".into(),
        actual_id: "act-joint".into(), actual_name: "Joint".into(),
    }];
    assert_eq!(resolve_transfer_dest(&TransferTarget::Pot("pot_abc".into()), &pots, &accts), Some("act-pot".to_string()));
    assert_eq!(resolve_transfer_dest(&TransferTarget::Account("acc_joint".into()), &pots, &accts), Some("act-joint".to_string()));
    assert_eq!(resolve_transfer_dest(&TransferTarget::Pot("pot_unmapped".into()), &pots, &accts), None);
}
```

- [ ] **Step 2: Run them; verify they fail.** Run: `cargo test transfer_payee_id_finds`; `cargo test resolve_transfer_dest_maps`. Expected: FAIL (functions not defined).

- [ ] **Step 3: Implement.** Add near `actual_id_for` in `src/run.rs`:

```rust
use crate::banks::TransferTarget;
use crate::state::PotLink;

/// Find the transfer payee id for an Actual account (a payee whose
/// `transfer_acct` equals `actual_id`). Actual auto-creates one per account.
fn transfer_payee_id<'a>(payees: &'a [crate::actual::shim_protocol::Payee], actual_id: &str) -> Option<&'a str> {
    payees
        .iter()
        .find(|p| p.transfer_acct.as_deref() == Some(actual_id))
        .map(|p| p.id.as_str())
}

/// Resolve a transfer's destination to an Actual account id. `Pot` looks up
/// `[[monzo.pots]]`; `Account` looks up `[[monzo.accounts]]`. `None` = unmapped.
fn resolve_transfer_dest(target: &TransferTarget, pots: &[PotLink], accounts: &[AccountLink]) -> Option<String> {
    match target {
        TransferTarget::Pot(pot_id) => pots
            .iter()
            .find(|p| &p.monzo_pot_id == pot_id && !p.actual_id.is_empty())
            .map(|p| p.actual_id.clone()),
        TransferTarget::Account(acc_id) => accounts
            .iter()
            .find(|a| &a.monzo_id == acc_id && !a.actual_id.is_empty())
            .map(|a| a.actual_id.clone()),
    }
}
```

(If `AccountLink`/`PotLink` are already imported at the top of `run.rs`, drop the redundant `use`. Run `cargo build` and let the compiler flag unused imports; finalize with fmt.)

- [ ] **Step 4: Run the tests; verify pass.** Run: `cargo test transfer_payee_id_finds`; `cargo test resolve_transfer_dest_maps`. Expected: PASS.

- [ ] **Step 5: Commit.**

```bash
git add src/run.rs
git commit -m "feat(run): pure transfer-resolution helpers"
```

---

## Task 7: Wire transfers into `run` (import + retry queue + summary)

This is the integration task. Transfers are partitioned out **before** the scheduled-match filter and Claude path; the retry queue is processed alongside newly fetched transfers.

**Files:**
- Modify: `src/run.rs` `run` (`src/run.rs:527-1060`) — partition, resolve, import, requeue, summary
- Test: `src/run.rs` tests module (integration test against the existing mock bank/actual/notifier)

- [ ] **Step 1: Write the failing integration test.** Use the existing test harness in `src/run.rs` tests (mirror an existing `run`-level test for the exact mock constructor names — e.g. `MockBank`, `MockActual`/recording client, `MockNotifier`; reuse whatever the closest existing test uses). The test asserts a mapped pot deposit imports as a transfer-payee row, and an unmapped one is held:

```rust
#[tokio::test]
async fn pot_deposit_imports_as_transfer_when_mapped() {
    // Arrange: one mapped current account + one mapped pot, one pot-deposit row.
    // (Follow the closest existing run() test for harness wiring; key assertions below.)
    let mut state = State::default();
    state.monzo.accounts.push(AccountLink {
        monzo_id: "acc_cur".into(), monzo_name: "Current".into(),
        actual_id: "act-cur".into(), actual_name: "Current".into(),
    });
    state.monzo.pots.push(PotLink {
        monzo_pot_id: "pot_abc".into(), monzo_pot_name: "Savings".into(),
        actual_id: "act-pot".into(), actual_name: "Savings".into(),
    });
    // Mock bank yields one pot-deposit RawTransaction:
    //   external_id "tx_1", source_monzo_id "acc_cur", amount -5000,
    //   transfer: Some(TransferTarget::Pot("pot_abc")).
    // Mock Actual get_payees returns a transfer payee:
    //   Payee{ id:"p_pot", name:"Transfer: Savings", transfer_acct:Some("act-pot") }.

    // Act: run(...).await.unwrap();

    // Assert: exactly one transaction imported into account "act-cur" with
    //   payee == "p_pot" and category == None; held_transfers empty.
}

#[tokio::test]
async fn pot_deposit_is_held_when_pot_unmapped() {
    // Same as above but state.monzo.pots is empty.
    // Assert: nothing imported; state.held_transfers has 1 entry with
    //   target_kind "pot", target_id "pot_abc"; no pending_reviews/Telegram prompt.
}
```

- [ ] **Step 2: Run them; verify they fail.** Run: `cargo test pot_deposit_imports_as_transfer_when_mapped pot_deposit_is_held_when_pot_unmapped`. Expected: FAIL (transfers currently flow into the ordinary path / no held queue handling).

- [ ] **Step 3: Implement the partition + processing.** In `run`, immediately after the fetch (`let fetched = …; let fetched_count = fetched.len();`, ~`src/run.rs:580`), split transfers out so they never reach the scheduled-match filter or Claude:

```rust
    // Phase 3: peel transfers off the ordinary pipeline. They route to a
    // destination account's transfer payee, never to payee/category classification.
    let (transfer_rows, fetched): (Vec<RawTransaction>, Vec<RawTransaction>) =
        fetched.into_iter().partition(|t| t.transfer.is_some());
```

(Keep using `fetched` for the existing ordinary path below — the binding is shadowed with the non-transfer rows.)

After `payees`/`categories` are loaded and `account_links` is cloned (the existing `let account_links = state.monzo.accounts.clone();`), and **before** the `grouped` rows are assembled into the import map, add the transfer-processing block. It feeds the SAME `grouped: BTreeMap<String, Vec<ImportTxn>>`:

```rust
    // ---- Phase 3: transfers (retry queue + newly fetched) -------------------
    let pot_links = state.monzo.pots.clone();
    // Build the work list: previously-held transfers (retry) + new ones.
    // Held entries reconstruct a minimal row; new entries come straight from the feed.
    let mut transfer_intents: Vec<HeldTransfer> = state.held_transfers.clone();
    for row in &transfer_rows {
        if let Some(ht) = HeldTransfer::from_row(row) {
            // Dedup against the retry queue we just cloned.
            if !transfer_intents.iter().any(|h| h.external_id == ht.external_id) {
                transfer_intents.push(ht);
            }
        }
    }

    let mut still_held: Vec<HeldTransfer> = Vec::new();
    let mut transfers_imported = 0usize;
    for ht in transfer_intents {
        let Some(target) = ht.target() else {
            warn!("held transfer {} has unknown target_kind '{}'; dropping", ht.external_id, ht.target_kind);
            continue;
        };
        let dest = resolve_transfer_dest(&target, &pot_links, &account_links);
        let src = actual_id_for(&ht.source_monzo_id, &account_links);
        match (src, dest) {
            (Some(src_actual), Some(dest_actual)) => {
                match transfer_payee_id(&payees, &dest_actual) {
                    Some(pid) => {
                        grouped.entry(src_actual).or_default().push(ImportTxn {
                            date: ht.date.clone(),
                            amount: ht.amount,
                            payee: pid.to_string(),
                            category: None, // transfers are uncategorised
                            imported_id: ht.external_id.clone(),
                        });
                        transfers_imported += 1;
                    }
                    None => {
                        // Dest mapped but Actual has no transfer payee for it
                        // (unexpected — Actual auto-creates them). Fail-loud: re-hold.
                        warn!("no transfer payee for account {dest_actual}; re-holding transfer {}", ht.external_id);
                        still_held.push(ht);
                    }
                }
            }
            _ => {
                // Source and/or destination unmapped → keep holding, retry next run.
                still_held.push(ht);
            }
        }
    }
    state.held_transfers = still_held;
```

In the **dry-run** branch, do not commit `held_transfers` changes (the dry-run path already returns after `persist_rotated_token`; just make sure the assignment above is fine to discard — it is, since dry-run reloads nothing). Add a dry-run print near the other `would …` lines:

```rust
        println!("  would transfer-import : {transfers_imported}");
        println!("  transfers still held  : {}", state.held_transfers.len());
```

In the **real** path, extend the final summary `body` (after the existing held line) and the `notify`:

```rust
    if transfers_imported > 0 {
        let _ = write!(body, "\nImported {transfers_imported} transfer(s).");
    }
    if !state.held_transfers.is_empty() {
        let _ = write!(body, "\n⤢ {} transfer(s) awaiting account mapping — run `aba accounts`.", state.held_transfers.len());
    }
```

- [ ] **Step 4: Run the two tests; verify pass.** Run: `cargo test pot_deposit_imports_as_transfer_when_mapped pot_deposit_is_held_when_pot_unmapped`. Expected: PASS. Iterate on harness wiring until green.

- [ ] **Step 5: Run the whole suite.** Run: `cargo test`. Expected: PASS (ordinary-path tests unaffected — they use `transfer: None`).

- [ ] **Step 6: clippy + fmt.** Run: `cargo clippy --all-targets -- -D warnings` then `cargo fmt --all`. Fix any lints. Re-run `cargo test`.

- [ ] **Step 7: Commit.**

```bash
git add src/run.rs
git commit -m "feat(run): import transfers via transfer payee + durable held-transfer retry"
```

---

## Task 8: Retry-when-mapped + idempotency integration test

**Files:**
- Test: `src/run.rs` tests module

- [ ] **Step 1: Write the failing test.**

```rust
#[tokio::test]
async fn held_transfer_imports_after_pot_is_mapped() {
    // Run 1: pot unmapped -> held (assert held_transfers.len()==1, 0 imported).
    // Map the pot: state.monzo.pots.push(PotLink{ ... actual_id:"act-pot" ... }).
    // Run 2: bank returns NO new rows (last_run advanced), but the held queue
    //   must still import the transfer now that the pot is mapped.
    // Assert: after run 2, exactly one transfer imported to "act-cur",
    //   state.held_transfers is empty.
}

#[tokio::test]
async fn held_transfer_not_double_imported_or_double_queued() {
    // Run 1: unmapped -> 1 held. Run 2 (same row re-fetched in overlap window,
    //   still unmapped): assert held_transfers.len() stays 1 (dedup by external_id).
}
```

- [ ] **Step 2: Run; verify they fail** (if the implementation is correct they may already pass — if so, that's acceptable; the test is the regression guard). Run: `cargo test held_transfer_imports_after_pot_is_mapped held_transfer_not_double_imported_or_double_queued`.

- [ ] **Step 3: If failing, fix `run` to satisfy them** (most likely already satisfied by Task 7's retry loop + `enqueue`/dedup). Ensure the second-run import uses the held queue even with an empty fetch.

- [ ] **Step 4: Run; verify pass.** Expected: PASS.

- [ ] **Step 5: Commit.**

```bash
git add src/run.rs
git commit -m "test(run): held transfer retry-when-mapped + idempotency"
```

---

## Task 9: `aba accounts` maps pots; RUNBOOK

**Files:**
- Modify: `src/run.rs` `suggest_links` (`src/run.rs:393-424`) → extract shared matcher; add `suggest_pot_links`; `map_accounts` (`src/run.rs:426-511`) pots block + `--write`
- Modify: `RUNBOOK.md`
- Test: `src/run.rs` tests module

- [ ] **Step 1: Write the failing test.**

```rust
#[test]
fn suggest_pot_links_matches_by_name_and_leaves_unmatched_unmapped() {
    let pots = vec![("pot_abc".to_string(), "Savings".to_string()),
                    ("pot_xyz".to_string(), "Holiday".to_string())];
    let actual = vec![("act-sav".to_string(), "Savings".to_string())];
    let links = suggest_pot_links(&pots, &actual, ACCOUNT_MATCH_THRESHOLD);
    assert_eq!(links.len(), 2);
    let sav = links.iter().find(|l| l.monzo_pot_id == "pot_abc").unwrap();
    assert_eq!(sav.actual_id, "act-sav");
    let hol = links.iter().find(|l| l.monzo_pot_id == "pot_xyz").unwrap();
    assert!(hol.actual_id.is_empty()); // no match -> unmapped
}
```

- [ ] **Step 2: Run it; verify it fails.** Run: `cargo test suggest_pot_links_matches`. Expected: FAIL (function not defined).

- [ ] **Step 3: Implement `suggest_pot_links`.** Add beside `suggest_links` in `src/run.rs`:

```rust
/// Auto-suggest Monzo-pot → Actual-account links by normalised-name similarity.
/// Same policy as `suggest_links`: best match above `threshold`, else unmapped.
fn suggest_pot_links(
    pots: &[(String, String)],
    actual: &[(String, String)],
    threshold: f64,
) -> Vec<PotLink> {
    pots.iter()
        .map(|(pid, pname)| {
            let best = actual
                .iter()
                .map(|(aid, aname)| (aid, aname, crate::matching::payee_similarity(pname, aname)))
                .filter(|(_, _, score)| *score >= threshold)
                .max_by(|a, b| a.2.total_cmp(&b.2));
            match best {
                Some((aid, aname, _)) => PotLink {
                    monzo_pot_id: pid.clone(),
                    monzo_pot_name: pname.clone(),
                    actual_id: aid.clone(),
                    actual_name: aname.clone(),
                },
                None => PotLink {
                    monzo_pot_id: pid.clone(),
                    monzo_pot_name: pname.clone(),
                    ..PotLink::default()
                },
            }
        })
        .collect()
}
```

- [ ] **Step 4: Run it; verify pass.** Run: `cargo test suggest_pot_links_matches`. Expected: PASS.

- [ ] **Step 5: Extend `map_accounts`.** Replace the informational pots block (`src/run.rs:485-498`, the `// Informational: pots are Phase 3` loop) with collection + suggestion:

```rust
    // Collect pots across all current accounts, then suggest Pot → Actual links.
    let mut all_pots: Vec<(String, String)> = Vec::new();
    for (mid, _) in &monzo_accounts {
        match bank.list_pots(mid).await {
            Ok(pots) => all_pots.extend(pots),
            Err(e) => warn!("could not list pots for {mid}: {}", bank_err_text(&e)),
        }
    }
    let pot_links = suggest_pot_links(&all_pots, &actual_accounts, ACCOUNT_MATCH_THRESHOLD);
    if !pot_links.is_empty() {
        println!("\nSuggested Monzo pot → Actual account mapping:");
        for l in &pot_links {
            if l.actual_id.is_empty() {
                println!("  {} ({})  →  [UNMAPPED — edit state file]", l.monzo_pot_name, l.monzo_pot_id);
            } else {
                println!("  {} ({})  →  {} ({})", l.monzo_pot_name, l.monzo_pot_id, l.actual_name, l.actual_id);
            }
        }
    }
```

Then in the `if write {` block, persist pots alongside accounts:

```rust
    if write {
        state.monzo.accounts = links;
        state.monzo.pots = pot_links;
        state.save(&cfg.state_path)?;
        println!(
            "\nWrote {} account link(s) and {} pot link(s) to {}.",
            state.monzo.accounts.len(),
            state.monzo.pots.len(),
            cfg.state_path.display()
        );
        println!("Review/override UNMAPPED or wrong rows by editing [[monzo.accounts]] / [[monzo.pots]] there.");
    } else {
```

- [ ] **Step 6: Update RUNBOOK.** In `RUNBOOK.md`, in the bootstrap section that documents `aba accounts`, add a paragraph: pots are now suggested and written to `[[monzo.pots]]`; create an Actual account per pot you want to track (on- or off-budget, your choice), then map it; unmapped pots' moves are held and retried until mapped (surfaced in the run summary). Keep wording consistent with the existing `[[monzo.accounts]]` instructions.

- [ ] **Step 7: Run the suite + clippy + fmt.** Run: `cargo test` (PASS), `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all`.

- [ ] **Step 8: Commit.**

```bash
git add src/run.rs RUNBOOK.md
git commit -m "feat(accounts): suggest+persist pot links; document in RUNBOOK"
```

---

## POTS SLICE GATE

- [ ] Run the full gate: `cargo test` (all green), `cargo clippy --all-targets -- -D warnings` (clean), `cargo fmt --all -- --check` (clean).
- [ ] Merge the pots slice to `main` (local-first; do NOT push unless the user asks). The branch model mirrors Phases 1–2: fast-forward/merge into `main`.
- [ ] STOP and report to the user before starting the inter-account slice, per the spec's "validate p2p-vs-FPS routing against the real feed first."

---

# INTER-ACCOUNT SLICE (Tasks 10–13)

> Prerequisite: confirm against a real Monzo feed whether same-user account→account moves arrive as `scheme: "p2p_payment"` (exposes `counterparty.account_id`) or as `payport_faster_payments` (only sort_code/account_number). The tasks below implement the **p2p path** (primary). If the real feed shows FPS routing, add a follow-up task to match on sort_code/account_number (would require capturing our own accounts' sort codes — out of scope until confirmed).

## Task 10: Parse `p2p_payment` counterparty into `TransferTarget::Account`

**Files:**
- Modify: `src/banks/monzo.rs` `MonzoTx`/`TxMetadata` + add `Counterparty`; `transfer_target` gains the p2p branch (needs our mapped account ids)
- Test: `src/banks/monzo.rs` tests

- [ ] **Step 1: Write the failing test.**

```rust
#[test]
fn classifies_p2p_to_our_account_as_account_transfer() {
    let tx = MonzoTx {
        id: "tx_3".into(), created: "2026-06-01T10:00:00Z".parse().unwrap(),
        amount: -2000, description: "".into(), decline_reason: None, merchant: None,
        scheme: "p2p_payment".into(),
        metadata: TxMetadata { pot_id: None },
        counterparty: Counterparty { account_id: Some("acc_joint".into()) },
    };
    let ours = ["acc_joint".to_string()];
    assert_eq!(tx.transfer_target_with(&ours), Some(TransferTarget::Account("acc_joint".into())));
}

#[test]
fn p2p_to_external_person_is_not_a_transfer() {
    let tx = MonzoTx {
        id: "tx_4".into(), created: "2026-06-01T10:00:00Z".parse().unwrap(),
        amount: -2000, description: "".into(), decline_reason: None, merchant: None,
        scheme: "p2p_payment".into(),
        metadata: TxMetadata { pot_id: None },
        counterparty: Counterparty { account_id: Some("acc_stranger".into()) },
    };
    let ours = ["acc_joint".to_string()];
    assert_eq!(tx.transfer_target_with(&ours), None);
}
```

- [ ] **Step 2: Run; verify fail.** Run: `cargo test classifies_p2p_to_our_account p2p_to_external_person`. Expected: FAIL.

- [ ] **Step 3: Implement.** Add `Counterparty` and the account-aware classifier; keep the old `transfer_target()` for pots delegating to the new one with an empty slice:

```rust
#[derive(Debug, Default, Deserialize)]
struct Counterparty {
    /// Destination Monzo account id on `p2p_payment` rows; matches `acc_…`.
    #[serde(default)]
    account_id: Option<String>,
}
```

Add `#[serde(default)] counterparty: Counterparty,` to `MonzoTx`. Replace `transfer_target` impl:

```rust
impl MonzoTx {
    /// Pots need no account context; convenience for pot-only call sites/tests.
    fn transfer_target(&self) -> Option<crate::banks::TransferTarget> {
        self.transfer_target_with(&[])
    }

    /// Classify as a transfer. `our_accounts` are our mapped Monzo account ids;
    /// a `p2p_payment` counts as an inter-account transfer only when its
    /// destination is one of them (otherwise it's an external person → ordinary).
    fn transfer_target_with(&self, our_accounts: &[String]) -> Option<crate::banks::TransferTarget> {
        if self.scheme == "uk_retail_pot" {
            if let Some(pot_id) = self.metadata.pot_id.clone() {
                return Some(crate::banks::TransferTarget::Pot(pot_id));
            }
        }
        if self.scheme == "p2p_payment" {
            if let Some(acc) = self.counterparty.account_id.clone() {
                if our_accounts.iter().any(|a| a == &acc) {
                    return Some(crate::banks::TransferTarget::Account(acc));
                }
            }
        }
        None
    }
}
```

- [ ] **Step 4: Pass our account ids into `fetch_account`.** The classifier call in the map closure must use `transfer_target_with`. `fetch_account` has `&self`, and `self.monzo.accounts` holds the map — build the slice once before the closure:

```rust
        let our_accounts: Vec<String> = self.monzo.accounts.iter().map(|l| l.monzo_id.clone()).collect();
        Ok(all
            .into_iter()
            .filter(|t| t.decline_reason.is_none())
            .map(|t| {
                let raw_merchant = /* unchanged */;
                let transfer = t.transfer_target_with(&our_accounts);
                RawTransaction { /* …, */ transfer }
            })
            .collect())
```

- [ ] **Step 5: Run new + full tests; verify pass.** Run: `cargo test`. Expected: PASS.

- [ ] **Step 6: Commit.**

```bash
git add src/banks/monzo.rs
git commit -m "feat(monzo): classify p2p_payment to a mapped account as Account transfer"
```

---

## Task 11: Suppress the inflow side (dedup)

**Files:**
- Modify: `src/run.rs` — drop the inflow counterpart before building transfer intents
- Test: `src/run.rs` tests

- [ ] **Step 1: Write the failing test.**

```rust
#[tokio::test]
async fn inter_account_transfer_imports_outflow_only() {
    // Two mapped accounts: acc_cur->act-cur, acc_joint->act-joint.
    // Bank returns BOTH feed sides of one move:
    //   outflow: external_id "tx_out", source acc_cur, amount -2000,
    //            transfer Account("acc_joint")
    //   inflow:  external_id "tx_in",  source acc_joint, amount +2000,
    //            transfer Account("acc_cur")
    // get_payees has transfer payees for both accounts.
    // Assert: exactly ONE import — the outflow into act-cur with the
    //   act-joint transfer payee; the inflow row is suppressed (not imported,
    //   not held). held_transfers empty.
}
```

- [ ] **Step 2: Run; verify fail** (both sides import or one is held). Run: `cargo test inter_account_transfer_imports_outflow_only`. Expected: FAIL.

- [ ] **Step 3: Implement suppression.** In `run`, right after the `partition` that produces `transfer_rows`, drop inflow counterparts of internal transfers before they become intents:

```rust
    // Inter-account moves appear in BOTH feeds. Keep the outflow (negative);
    // suppress the inflow (positive) whose destination is one of our accounts —
    // Actual creates that side itself when it links the outflow's transfer.
    let our_monzo_ids: std::collections::BTreeSet<String> =
        state.monzo.accounts.iter().map(|l| l.monzo_id.clone()).collect();
    let transfer_rows: Vec<RawTransaction> = transfer_rows
        .into_iter()
        .filter(|t| {
            match &t.transfer {
                Some(TransferTarget::Account(dest)) => {
                    // inflow counterpart = positive amount AND dest is ours
                    !(t.amount_minor > 0 && our_monzo_ids.contains(dest))
                }
                _ => true, // pots and outflows pass through
            }
        })
        .collect();
```

Place this immediately after the `partition` line from Task 7 (so the retry/intent logic only ever sees outflows + pots).

- [ ] **Step 4: Run; verify pass.** Run: `cargo test inter_account_transfer_imports_outflow_only`. Then `cargo test`. Expected: PASS.

- [ ] **Step 5: clippy + fmt.** Run: `cargo clippy --all-targets -- -D warnings`; `cargo fmt --all`.

- [ ] **Step 6: Commit.**

```bash
git add src/run.rs
git commit -m "feat(run): suppress inflow side of inter-account transfers (dedup)"
```

---

## Task 12: Inter-account held + retry e2e

**Files:**
- Test: `src/run.rs` tests

- [ ] **Step 1: Write the test.** An outflow to a mapped account imports as a transfer; an outflow to a Monzo account that exists in the map but is **unmapped** (`actual_id` empty) is held and retried when mapped:

```rust
#[tokio::test]
async fn inter_account_outflow_held_until_dest_mapped() {
    // acc_joint present in accounts but actual_id == "" (unmapped).
    // Run 1: outflow acc_cur -> acc_joint held (held_transfers len 1, 0 imported).
    // Map acc_joint (set actual_id "act-joint"); add its transfer payee.
    // Run 2 (no new fetch): held transfer imports into act-cur; queue empties.
}
```

- [ ] **Step 2: Run; verify it passes** (logic from Tasks 7+10+11 should already cover it; this is the regression guard). Run: `cargo test inter_account_outflow_held_until_dest_mapped`. Fix `run` if needed.

- [ ] **Step 3: Commit.**

```bash
git add src/run.rs
git commit -m "test(run): inter-account transfer held-until-mapped retry"
```

---

## Task 13: Inter-account slice gate + docs

- [ ] **Step 1: Full gate.** `cargo test` (green), `cargo clippy --all-targets -- -D warnings` (clean), `cargo fmt --all -- --check` (clean).
- [ ] **Step 2: RUNBOOK note.** Add a short note that Monzo→Monzo transfers between two mapped accounts are auto-linked (one imported row per move; the duplicate feed side is suppressed), and that transfers to accounts you don't map are treated as ordinary payments.
- [ ] **Step 3: Commit docs.**

```bash
git add RUNBOOK.md
git commit -m "docs(runbook): document inter-account transfer behavior"
```

- [ ] **Step 4: Merge the inter-account slice to `main`** (local-first; push only on the user's explicit request).

---

## Self-Review (completed during authoring)

- **Spec coverage:** §1 data model → Tasks 1,2,4; §1 detection → Tasks 3,10; §2 fetch → Tasks 3,10; §3 import/shim → Tasks 5,6,7; §4 dedup → Task 11; §5 held queue → Tasks 4,7,8,12; §6 mapping UX → Task 9; §7 tests → distributed (Tasks 3,6,7,8,9,11,12). All covered.
- **Type consistency:** `TransferTarget::{Pot,Account}`, `PotLink{monzo_pot_id,monzo_pot_name,actual_id,actual_name}`, `HeldTransfer{external_id,source_monzo_id,date,amount,target_kind,target_id,raw_merchant}`, `Payee.transfer_acct: Option<String>`, helpers `transfer_payee_id`/`resolve_transfer_dest`/`suggest_pot_links`, `MonzoTx::transfer_target_with` — names consistent across all tasks.
- **Placeholders:** none — every code step shows the code; test harness wiring (Task 7/8/11/12) intentionally defers to the closest existing `run()` test for mock constructor names, since those are established patterns in the tree the implementer will mirror.
- **Ordering:** pots fully shippable at the Task 9 gate before any inter-account work, per the chosen packaging.
