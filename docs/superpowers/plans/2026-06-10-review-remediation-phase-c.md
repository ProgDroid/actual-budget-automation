# Phase C — ID Newtypes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task (inline execution chosen). Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the four interchangeable `String` identifiers (`monzo_id`, `pot_id`, Actual account id, bank `external_id`) with distinct newtypes so the compiler rejects mixing them, and so "unmapped" stops being an empty-string sentinel.

**Architecture:** A new `src/ids.rs` defines `MonzoAccountId`, `MonzoPotId`, `ActualAccountId`, `ExternalTxId` — each a `#[serde(transparent)]` wrapper over `String`. Validation lives **only** in smart constructors used at ingestion boundaries (Monzo API parse, Actual shim parse); `Deserialize` is intentionally unvalidated so a persisted `state.json` or shim response never fails to load. Wire formats (state file, Node shim JSON) stay byte-identical. The empty-string "unmapped" sentinel on `AccountLink.actual_id` / `PotLink.actual_id` becomes `Option<ActualAccountId>`, with a `""`⇄`None` serde adapter to preserve old state files. Threading is **full end-to-end**: domain structs, bank/transfer types, shim types, the `(id, payee, category)` pipeline tuples, and `ActualClient` signatures. Zero behaviour change.

**Tech Stack:** Rust 2024, serde (transparent + custom `with`), thiserror, `cargo test` (159/155 tests are the regression net).

---

## Design Decisions (confirmed)

1. **Validating smart constructors at the edges, transparent serde at rest.** `new()`/`TryFrom` validate; `Serialize`/`Deserialize` bypass validation. This keeps it a pure type-safety refactor with no risk of bricking a deployed `state.json`.
2. **Full end-to-end threading**, including pipeline-internal tuples and `ActualClient` method signatures.
3. **`Option<ActualAccountId>` replaces the empty-string "unmapped" sentinel** — the natural consequence of giving `ActualAccountId` a non-empty constructor. A `""`⇄`None` serde adapter preserves state-file compatibility.
4. **Scope = exactly the four named ids.** Config secrets (`actual_sync_id`, `monzo_client_id`, …) and Actual payee/category ids are **out of scope** — they are not subject to the mix-up bug class and have no named newtype.

## File Structure

- **Create** `src/ids.rs` — the four newtypes, `IdError`, smart constructors, `Display`/`as_str`/`into_inner`, the `opt_actual_empty` serde adapter, and unit tests (validation + serde transparency + `""`⇄`None`).
- **Modify** `src/main.rs` (or lib root) — add `mod ids;` and re-exports.
- **Modify** `src/state.rs` — `MonzoState.account_id`→`MonzoAccountId`; `AccountLink.{monzo_id→MonzoAccountId, actual_id→Option<ActualAccountId>}`; `PotLink.{monzo_pot_id→MonzoPotId, actual_id→Option<ActualAccountId>}`; `migrate_legacy_account`, `take_resolved`, and the pending/held helpers that touch ids.
- **Modify** `src/banks/mod.rs` — `RawTransaction.{external_id→ExternalTxId, source_monzo_id→MonzoAccountId}`; `TransferTarget::{Pot(MonzoPotId), Account(MonzoAccountId)}`.
- **Modify** `src/banks/monzo.rs` — construct ids at ingestion (`MonzoTx.id`, fetched account id, `metadata.pot_id`, flex account id, `MonzoAccount.id`, `MonzoPot.id`); `transfer_target*` signatures.
- **Modify** `src/actual/shim_protocol.rs` — `Account.id→ActualAccountId`; `ImportTxn.imported_id→ExternalTxId`.
- **Modify** `src/actual/mod.rs` — `ActualClient` methods that accept/return account ids.
- **Modify** `src/pipeline/{import,classify,transfer,accounts,mod,testutil}.rs` — the `(String,String,String)` cache/match tuples → `(ExternalTxId,String,String)`; `Assembled.grouped` key → `ActualAccountId`; transfer/accounts resolution against the new `Option<ActualAccountId>`.
- **Modify** `src/poll.rs`, `src/telegram/prompts.rs`, `src/report.rs` — propagate id types where they surface.

---

## Task 1: The `ids` module (TDD — this is the only task with genuinely new behaviour)

**Files:**
- Create: `src/ids.rs`
- Modify: `src/main.rs` (add `mod ids;`)

- [ ] **Step 1: Write `src/ids.rs` with the four newtypes, `IdError`, constructors, and the serde adapter.**

```rust
//! Strongly-typed identifiers.
//!
//! Each id wraps a `String` but is a distinct type, so the compiler rejects
//! mixing (e.g. a Monzo account id where an Actual account id is wanted).
//!
//! **Serde is `transparent`**: on the wire (`state.json`, the Node shim JSON
//! protocol) each id is just its inner string — byte-identical to the
//! pre-newtype representation. Validation lives ONLY in the smart constructors
//! (`new`), which are called at ingestion boundaries (Monzo / Actual JSON parse).
//! `Deserialize` is deliberately NOT validated, so a persisted state file or a
//! shim response never fails to load. `Default` likewise yields an unvalidated
//! empty id for struct-update ergonomics in tests.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Error from a validating id constructor.
#[derive(thiserror::Error, Debug, PartialEq, Eq)]
pub enum IdError {
    #[error("expected a Monzo account id starting with `acc_`, got {0:?}")]
    MonzoAccount(String),
    #[error("expected a Monzo pot id starting with `pot_`, got {0:?}")]
    MonzoPot(String),
    #[error("external transaction id must not be empty")]
    EmptyExternalTx,
    #[error("Actual account id must not be empty")]
    EmptyActualAccount,
}

macro_rules! string_newtype {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(
            Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash,
            Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// The id as a string slice.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
            /// Consume the newtype, returning the inner `String`.
            #[must_use]
            pub fn into_inner(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

string_newtype! {
    /// Monzo account id (`acc_…`).
    MonzoAccountId
}
string_newtype! {
    /// Monzo pot id (`pot_…`).
    MonzoPotId
}
string_newtype! {
    /// Actual account UUID — the import / transfer target.
    ActualAccountId
}
string_newtype! {
    /// A bank's stable transaction id; used as Actual's `imported_id`.
    ExternalTxId
}

impl MonzoAccountId {
    /// Construct from a raw string, validating the `acc_` prefix.
    ///
    /// # Errors
    /// Returns [`IdError::MonzoAccount`] if `s` does not start with `acc_`.
    pub fn new(s: impl Into<String>) -> Result<Self, IdError> {
        let s = s.into();
        if s.starts_with("acc_") {
            Ok(Self(s))
        } else {
            Err(IdError::MonzoAccount(s))
        }
    }
}

impl MonzoPotId {
    /// Construct from a raw string, validating the `pot_` prefix.
    ///
    /// # Errors
    /// Returns [`IdError::MonzoPot`] if `s` does not start with `pot_`.
    pub fn new(s: impl Into<String>) -> Result<Self, IdError> {
        let s = s.into();
        if s.starts_with("pot_") {
            Ok(Self(s))
        } else {
            Err(IdError::MonzoPot(s))
        }
    }
}

impl ActualAccountId {
    /// Construct from a raw string, rejecting the empty string (which would
    /// collide with the historical "unmapped" sentinel — now modelled as
    /// `Option::None`).
    ///
    /// # Errors
    /// Returns [`IdError::EmptyActualAccount`] if `s` is empty.
    pub fn new(s: impl Into<String>) -> Result<Self, IdError> {
        let s = s.into();
        if s.is_empty() {
            Err(IdError::EmptyActualAccount)
        } else {
            Ok(Self(s))
        }
    }
}

impl ExternalTxId {
    /// Construct from a raw string, rejecting the empty string.
    ///
    /// # Errors
    /// Returns [`IdError::EmptyExternalTx`] if `s` is empty.
    pub fn new(s: impl Into<String>) -> Result<Self, IdError> {
        let s = s.into();
        if s.is_empty() {
            Err(IdError::EmptyExternalTx)
        } else {
            Ok(Self(s))
        }
    }
}

/// Serde adapter for `Option<ActualAccountId>` fields whose on-disk form is a
/// string where `""` means "unmapped". Preserves byte-compatibility with state
/// files written before the `Option` modelling.
pub mod opt_actual_empty {
    use super::ActualAccountId;
    use serde::{Deserialize, Deserializer, Serializer};

    /// # Errors
    /// Propagates the serializer's error.
    pub fn serialize<S: Serializer>(
        v: &Option<ActualAccountId>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        match v {
            Some(id) => s.serialize_str(id.as_str()),
            None => s.serialize_str(""),
        }
    }

    /// # Errors
    /// Propagates the deserializer's error.
    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<ActualAccountId>, D::Error> {
        let s = String::deserialize(d)?;
        Ok(if s.is_empty() {
            None
        } else {
            // Bypass validation: trust what was previously persisted.
            Some(ActualAccountId(s))
        })
    }
}
```

- [ ] **Step 2: Append the test module to `src/ids.rs`.**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monzo_account_validates_prefix() {
        assert!(MonzoAccountId::new("acc_123").is_ok());
        assert_eq!(
            MonzoAccountId::new("pot_123"),
            Err(IdError::MonzoAccount("pot_123".to_owned()))
        );
    }

    #[test]
    fn monzo_pot_validates_prefix() {
        assert!(MonzoPotId::new("pot_123").is_ok());
        assert!(MonzoPotId::new("acc_123").is_err());
    }

    #[test]
    fn actual_and_external_reject_empty() {
        assert!(ActualAccountId::new("uuid").is_ok());
        assert_eq!(ActualAccountId::new(""), Err(IdError::EmptyActualAccount));
        assert!(ExternalTxId::new("tx_1").is_ok());
        assert_eq!(ExternalTxId::new(""), Err(IdError::EmptyExternalTx));
    }

    #[test]
    fn serde_is_transparent() {
        let id = MonzoAccountId::new("acc_9").expect("valid");
        let json = serde_json::to_string(&id).expect("ser");
        assert_eq!(json, "\"acc_9\"");
        let back: MonzoAccountId = serde_json::from_str(&json).expect("de");
        assert_eq!(back, id);
    }

    #[test]
    fn deserialize_does_not_validate() {
        // A persisted-but-malformed id must still load (no brick).
        let back: MonzoAccountId = serde_json::from_str("\"garbage\"").expect("de");
        assert_eq!(back.as_str(), "garbage");
    }

    #[test]
    fn opt_actual_empty_roundtrips_sentinel() {
        #[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
        struct Holder {
            #[serde(with = "opt_actual_empty")]
            actual_id: Option<ActualAccountId>,
        }
        // "" -> None
        let none: Holder = serde_json::from_str(r#"{"actual_id":""}"#).expect("de");
        assert_eq!(none.actual_id, None);
        // None -> ""  (byte-identical to old files)
        assert_eq!(
            serde_json::to_string(&none).expect("ser"),
            r#"{"actual_id":""}"#
        );
        // populated roundtrips
        let some: Holder =
            serde_json::from_str(r#"{"actual_id":"uuid-1"}"#).expect("de");
        assert_eq!(some.actual_id.as_ref().map(ActualAccountId::as_str), Some("uuid-1"));
    }
}
```

- [ ] **Step 3: Register the module.** In `src/main.rs`, add `mod ids;` alongside the other `mod` declarations (keep alphabetical/existing order; the Edit hook reorders case-insensitively, so place it where `ids` sorts).

- [ ] **Step 4: Run the new tests.**

Run: `cargo test --no-default-features ids::`
Expected: PASS (6 tests). `serde_json` is already a dependency (used by shim_protocol/state); if `cargo check` reports it missing as a dev-dep, add `serde_json` under `[dev-dependencies]` — but it is almost certainly already a normal dependency.

- [ ] **Step 5: Commit.**

```bash
git add src/ids.rs src/main.rs
git commit -m "feat(ids): add MonzoAccountId/MonzoPotId/ActualAccountId/ExternalTxId newtypes"
```

---

## Task 2: Thread ids through `state.rs`

**Files:**
- Modify: `src/state.rs` (structs `MonzoState`, `AccountLink`, `PotLink`; methods `migrate_legacy_account`, `take_resolved`, pending/held helpers; `tests` module)

- [ ] **Step 1: Change the struct field types.**

```rust
// MonzoState
pub account_id: MonzoAccountId,   // was String (legacy migration field; Default = empty)

// AccountLink
pub monzo_id: MonzoAccountId,                                   // was String
#[serde(default, with = "crate::ids::opt_actual_empty")]
pub actual_id: Option<ActualAccountId>,                         // was String ("" = unmapped)

// PotLink
pub monzo_pot_id: MonzoPotId,                                   // was String
#[serde(default, with = "crate::ids::opt_actual_empty")]
pub actual_id: Option<ActualAccountId>,                         // was String ("" = unmapped)
```

Add `use crate::ids::{ActualAccountId, MonzoAccountId, MonzoPotId};` at the top of `state.rs`.

- [ ] **Step 2: Fix `migrate_legacy_account` and every `actual_id`/`monzo_id` read.** Run `cargo check --no-default-features` and resolve each error:
  - Empty-check sites (`actual_id.is_empty()`) become `actual_id.is_none()`; "mapped" reads become `if let Some(id) = &link.actual_id`.
  - `migrate_legacy_account` wraps the legacy `account_id` (already a `MonzoAccountId` after Step 1) when seeding the first `AccountLink`; `actual_id` for the migrated link is `None`.
  - `take_resolved` and the pending/held helpers: wrap/destructure ids as needed. Keep behaviour identical.

- [ ] **Step 3: Update `state.rs` tests.** Construct ids via `MonzoAccountId::new("acc_…").unwrap()` etc. in test fixtures; `actual_id` fixtures become `Some(ActualAccountId::new("…").unwrap())` or `None`. Keep the existing legacy-migration and version-guard test assertions semantically unchanged.

- [ ] **Step 4: Verify.**

Run: `cargo test --no-default-features state::`
Expected: PASS (all state tests, including legacy-migration round-trip and the `""`⇄`None` on-disk compatibility).

- [ ] **Step 5: Commit.**

```bash
git add src/state.rs
git commit -m "refactor(state): newtype account/pot ids; model unmapped actual_id as Option"
```

---

## Task 3: Thread ids through the bank layer

**Files:**
- Modify: `src/banks/mod.rs` (`RawTransaction`, `TransferTarget`)
- Modify: `src/banks/monzo.rs` (ingestion construction, `transfer_target*` signatures, tests)

- [ ] **Step 1: Change `banks/mod.rs` types.**

```rust
// RawTransaction
pub external_id: ExternalTxId,        // was String
pub source_monzo_id: MonzoAccountId,  // was String

// TransferTarget
pub enum TransferTarget {
    Pot(MonzoPotId),       // was String
    Account(MonzoAccountId), // was String
    FlexMirror,
}
```

Add `use crate::ids::{ExternalTxId, MonzoAccountId, MonzoPotId};`.

- [ ] **Step 2: Construct ids at Monzo ingestion in `monzo.rs`.** Compiler-driven; the key sites:
  - `MonzoTx.id` → `ExternalTxId` when building each `RawTransaction.external_id`. The id comes from Monzo and is non-empty in practice; on the empty/error case, **fail loud** per the fetch-fail-loud invariant — map `IdError` into `BankError` (add a `BankError` variant or reuse the existing parse/error variant) rather than dropping the row.
  - fetched account id and `flex_account_id` → `MonzoAccountId` (validate via `MonzoAccountId::new`, mapping `IdError`→`BankError`).
  - `metadata.pot_id` → `MonzoPotId` inside `transfer_target` (`TransferTarget::Pot`).
  - `MonzoAccount.id` → `MonzoAccountId` and `MonzoPot.id` → `MonzoPotId` where the accounts/pots lists are turned into links/suggestions.
  - `transfer_target_flex(&self, fetched_account_id: &MonzoAccountId, flex_account_id: Option<&MonzoAccountId>)` — update the signature; comparison `fid != fetched_account_id` stays (newtypes derive `Eq`).

- [ ] **Step 3: Update `monzo.rs` tests** to build ids via constructors, and assert the same `TransferTarget` outcomes.

- [ ] **Step 4: Verify.**

Run: `cargo test --no-default-features monzo::`
Expected: PASS (scheme/transfer classification + accounts/pots parsing tests).

- [ ] **Step 5: Commit.**

```bash
git add src/banks/mod.rs src/banks/monzo.rs
git commit -m "refactor(banks): newtype external/account/pot ids in RawTransaction + TransferTarget"
```

---

## Task 4: Thread ids through the Actual layer

**Files:**
- Modify: `src/actual/shim_protocol.rs` (`Account.id`, `ImportTxn.imported_id`)
- Modify: `src/actual/mod.rs` (`ActualClient` methods accepting/returning account ids)

- [ ] **Step 1: Change shim types.**

```rust
// Account  (deserialized from the shim's getAccounts) — transparent, so wire-identical
pub id: ActualAccountId,   // was String

// ImportTxn  (serialized to the shim) — transparent, so wire-identical
pub imported_id: ExternalTxId,  // was String
// `payee` and `category` stay String: they are Actual payee/category ids, out of scope.
```

Add `use crate::ids::{ActualAccountId, ExternalTxId};`.

- [ ] **Step 2: Update `ActualClient` method signatures** that take or return an Actual account id (e.g. import-by-account, account lookup) to `ActualAccountId`. Compiler-driven. The `getAccounts` deserialize path already yields `ActualAccountId` via `Account.id`.

- [ ] **Step 3: Update `actual/mod.rs` + `shim_protocol.rs` tests** to use `ActualAccountId::new(...).unwrap()` / `ExternalTxId::new(...).unwrap()`. Assert the serialized `ImportTxn` JSON is unchanged (transparent).

- [ ] **Step 4: Verify.**

Run: `cargo test --no-default-features actual::`
Expected: PASS (shim request/response (de)serialization, including the flat `ShimResponse` `ok` test).

- [ ] **Step 5: Commit.**

```bash
git add src/actual/mod.rs src/actual/shim_protocol.rs
git commit -m "refactor(actual): newtype Account.id (ActualAccountId) + ImportTxn.imported_id (ExternalTxId)"
```

---

## Task 5: Thread ids through the pipeline + leaf modules

**Files:**
- Modify: `src/pipeline/import.rs` (`AssembleInputs` tuples, `Assembled.grouped` key, `assemble`, `hold_unmapped`)
- Modify: `src/pipeline/classify.rs` (`apply_matches`, `plan_resolution`, `MatchOutcome`, the `(id,payee,cat)` tuples)
- Modify: `src/pipeline/transfer.rs` (`actual_id_for`, `resolve_transfer_dest`, `reconcile_flex_mirrors`, `resolve_intents`)
- Modify: `src/pipeline/accounts.rs` (`map_accounts`, `suggest_*`)
- Modify: `src/pipeline/mod.rs`, `src/pipeline/testutil.rs`
- Modify: `src/poll.rs`, `src/telegram/prompts.rs`, `src/report.rs`

- [ ] **Step 1: Update the shared tuples.** The cache/match tuples carrying an external id become typed:

```rust
// AssembleInputs
pub cached: &'a [(ExternalTxId, String, String)],   // (external_id, payee_name, category_name)
pub matched: &'a [(ExternalTxId, String, String)],
// Assembled
pub grouped: BTreeMap<ActualAccountId, Vec<ImportTxn>>,  // keyed by Actual account id
```

`ActualAccountId` derives `Ord`+`Hash`, so it is a valid `BTreeMap` key.

- [ ] **Step 2: Resolve transfer/account resolution against `Option<ActualAccountId>`.** In `transfer.rs::actual_id_for` and `accounts.rs::map_accounts`, the "is this link mapped?" checks switch from `!actual_id.is_empty()` to `actual_id.is_some()` / `if let Some(id) = &link.actual_id`. `resolve_transfer_dest` returns/handles `ActualAccountId`. `import.rs::hold_unmapped` holds rows whose resolved dest is `None`.

- [ ] **Step 3: Propagate through `mod.rs`, `poll.rs`, `telegram/prompts.rs`, `report.rs`, `testutil.rs`.** Compiler-driven: wherever an id string was passed, pass the newtype; where a human-readable string is needed (Telegram messages, report lines, log lines), call `.as_str()` / `Display`. `testutil.rs` fixtures build ids via constructors.

- [ ] **Step 4: Update all affected `#[cfg(test)] mod tests`** in the pipeline modules to construct ids via the constructors and assert identical grouping/resolution outcomes.

- [ ] **Step 5: Verify the whole pipeline.**

Run: `cargo test --no-default-features pipeline::`
Expected: PASS (including the ~24 integration tests in `pipeline/mod.rs`).

- [ ] **Step 6: Commit.**

```bash
git add src/pipeline src/poll.rs src/telegram/prompts.rs src/report.rs
git commit -m "refactor(pipeline): thread id newtypes through assembly/classify/transfer/accounts + leaves"
```

---

## Task 6: Full gate + cleanup

- [ ] **Step 1: Format.** Run: `cargo fmt --all`
- [ ] **Step 2: Type-check both feature configs.**
  Run: `cargo check --no-default-features` then `cargo check --all-features`
  Expected: clean (the `auth-flow` feature paths also compile against the new ids).
- [ ] **Step 3: Clippy auto-fix then enforce.**
  Run: `cargo clippy --fix --allow-dirty --all-targets --no-default-features`
  Then: `cargo clippy --all-targets --no-default-features -- -D warnings`
  Expected: zero warnings. Watch for `redundant_pub_crate` (use `pub` not `pub(crate)` in private `ids` items if it fires) and any new `must_use`/`missing_errors_doc` asks (the constructors already carry `# Errors`).
- [ ] **Step 4: Full test suite, both configs.**
  Run: `cargo test --no-default-features` (expect 155) then `cargo test --all-features` (expect 159).
  Expected: all green, counts unchanged from the Phase B baseline (no tests deleted; some gained id-construction lines).
- [ ] **Step 5: Format check (gate parity with CI).**
  Run: `cargo fmt --all -- --check`
  Expected: clean.
- [ ] **Step 6: Final commit.**

```bash
git add -A
git commit -m "chore(ids): clippy/fmt gate for Phase C id-newtype threading"
```

---

## Self-Review

- **Spec coverage:** All four named newtypes created (Task 1) and threaded into every site enumerated in the file-structure map: state (T2), banks (T3), actual (T4), pipeline+leaves (T5). Gate (T6). ✅
- **Wire compatibility:** state.json preserved via `#[serde(transparent)]` + `opt_actual_empty` adapter (T1 test `opt_actual_empty_roundtrips_sentinel`, T2 step 4); shim JSON preserved via transparent `Account`/`ImportTxn` (T4 step 3). ✅
- **No-brick guarantee:** `Deserialize` unvalidated (T1 test `deserialize_does_not_validate`). ✅
- **Fail-loud:** Monzo-side `IdError`→`BankError` mapping (T3 step 2) keeps the fetch-fail-loud invariant — a malformed id at ingestion errors the run rather than silently dropping a row. ✅
- **Type consistency:** constructor name is `new` on all four; accessor `as_str`/`into_inner`/`Display` uniform; `Option<ActualAccountId>` used consistently in `AccountLink`/`PotLink` and the resolution paths. ✅
- **Out of scope confirmed:** config secrets and Actual payee/category ids deliberately untouched. ✅
