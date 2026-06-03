# Multi-Account Routing & `account_id` Split — Implementation Plan (Phase 1)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Split the overloaded `monzo.account_id` into per-account Monzo→Actual links, fetch from every configured Monzo account, route each transaction to the correct Actual account on import, and add an `aba accounts` command that auto-suggests the mapping.

**Architecture:** A new `AccountLink { monzo_id, monzo_name, actual_id, actual_name }` lives in `MonzoState.accounts`. `RawTransaction` carries its `source_monzo_id`; `PendingReview`/`ResolvedReview` carry it too so held-then-resolved items still route. `monzo.rs` fetches per account; `run.rs` groups import rows by `actual_id` and imports once per Actual account, holding fail-loud anything unmapped. `aba accounts` fetches both sides (new `get_accounts` shim op) and auto-suggests links by normalised-name similarity.

**Tech Stack:** Rust (tokio, async-trait, serde, anyhow, reqwest, wiremock for tests, strsim via `matching`), Node `@actual-app/api` shim.

**Spec:** `docs/superpowers/specs/2026-06-03-multi-account-routing-design.md`

**Conventions (from project memory):**
- Build: `cargo build`. Test: `cargo test` (NOT `cargo test --lib`). Lint: `cargo clippy --all-targets -- -D warnings` (pedantic+nursery enforced via `[lints.clippy]`).
- Windows dev box: invoke the test binary directly via cargo; do not assume `.cmd` spawning.
- Fail-loud invariant: a fetch/import must return `Err`, never a partial `Ok`. `last_run` only advances on full success.

---

## File structure

| File | Responsibility | Change |
|------|----------------|--------|
| `src/banks/mod.rs` | `RawTransaction` shape | Add `source_monzo_id` |
| `src/state.rs` | persisted state | Add `AccountLink`, `MonzoState.accounts`, migration, `source_monzo_id` on `PendingReview`/`ResolvedReview`, thread through `add_pending*`, fix `monzo_clone` |
| `src/actual/shim_protocol.rs` | shim wire protocol | Add `GetAccounts` op + `Account` struct |
| `src/actual/mod.rs` | shim client | Add `get_accounts()` |
| `actual-shim/index.js` | Node shim | Add `get_accounts` case |
| `src/banks/monzo.rs` | Monzo fetch + discovery | Per-account fetch + tag; add `list_accounts`/`list_pots`; update guards/tests |
| `src/run.rs` | orchestration | Route imports by `actual_id`; add `actual_id_for`; add `map_accounts` command fn |
| `src/config.rs` | CLI | Add `Accounts { write }` command variant |
| `src/main.rs` | dispatch | Wire `Command::Accounts` |
| `docs/RUNBOOK.md` | ops docs | Document `aba accounts` + state migration |

---

## Task 1: Add `source_monzo_id` to `RawTransaction`

**Files:**
- Modify: `src/banks/mod.rs` (struct + its unit test)
- Modify: `src/banks/monzo.rs:229-234` (production construction), test helpers
- Modify: `src/matching.rs:85-92` (test helper)
- Modify: `src/run.rs:680-687` (test helper)

- [ ] **Step 1: Extend the struct**

In `src/banks/mod.rs`, replace the `RawTransaction` struct body with:

```rust
/// A transaction as fetched from a bank, before payee normalisation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawTransaction {
    /// Bank's stable id; used as Actual's `imported_id` for idempotency.
    pub external_id: String,
    pub date: NaiveDate,
    /// Integer minor units (pence). Negative = outflow.
    pub amount_minor: i64,
    /// Best raw merchant string (merchant name, fallback description).
    pub raw_merchant: String,
    /// Monzo account id this transaction was fetched from; routes the import.
    pub source_monzo_id: String,
}
```

- [ ] **Step 2: Build to find every construction site**

Run: `cargo build`
Expected: FAIL — `missing field source_monzo_id` at 4 sites (monzo.rs prod, mod.rs test, matching.rs test helper, run.rs test helper).

- [ ] **Step 3: Fix the production construction**

In `src/banks/monzo.rs`, in `fetch_since`'s mapping closure (the `RawTransaction { external_id: t.id, … }` literal), add the field. NOTE: Task 4 moves this into a per-account helper that has `monzo_id` in scope; for now use the existing single account id:

```rust
                RawTransaction {
                    external_id: t.id,
                    date: t.created.date_naive(),
                    amount_minor: t.amount,
                    raw_merchant,
                    source_monzo_id: self.monzo.account_id.clone(),
                }
```

- [ ] **Step 4: Fix the three test helpers**

`src/banks/mod.rs` unit test literal: add `source_monzo_id: "acc_1".into(),`.
`src/matching.rs` `fn tx(...)`: add `source_monzo_id: "acc_1".into(),`.
`src/run.rs` `pub fn tx(...)`: add `source_monzo_id: "acc_1".into(),`.

- [ ] **Step 5: Verify build + tests green**

Run: `cargo build` then `cargo test`
Expected: PASS (no behaviour changed yet).

- [ ] **Step 6: Commit**

```bash
git add src/banks/mod.rs src/banks/monzo.rs src/matching.rs src/run.rs
git commit -m "feat(banks): tag RawTransaction with source_monzo_id"
```

---

## Task 2: State model — `AccountLink`, migration, threaded source id

**Files:**
- Modify: `src/state.rs` (`MonzoState`, new `AccountLink`, `State::load` migration, `monzo_clone`, `PendingReview`, `ResolvedReview`, `add_pending`, `add_pending_full`, `take_resolved`, tests)

- [ ] **Step 1: Write failing migration + routing-field tests**

Add to the `tests` module in `src/state.rs`:

```rust
#[test]
fn legacy_account_id_migrates_to_unmapped_link() {
    let toml = r#"
        [monzo]
        refresh_token = "rt"
        account_id = "acc_legacy"
    "#;
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), toml).unwrap();
    let s = State::load(tmp.path()).unwrap();
    assert_eq!(s.monzo.account_id, "", "legacy field cleared");
    assert_eq!(s.monzo.accounts.len(), 1);
    assert_eq!(s.monzo.accounts[0].monzo_id, "acc_legacy");
    assert_eq!(s.monzo.accounts[0].actual_id, "", "unmapped until `aba accounts`");
}

#[test]
fn accounts_round_trip_and_no_double_migrate() {
    let mut s = State::default();
    s.monzo.accounts.push(AccountLink {
        monzo_id: "acc_1".into(),
        monzo_name: "Personal".into(),
        actual_id: "uuid-1".into(),
        actual_name: "Monzo Personal".into(),
    });
    let tmp = tempfile::NamedTempFile::new().unwrap();
    s.save(tmp.path()).unwrap();
    let loaded = State::load(tmp.path()).unwrap();
    assert_eq!(loaded.monzo.accounts, s.monzo.accounts);
}

#[test]
fn add_pending_stores_source_monzo_id() {
    let mut s = State::default();
    assert!(s.add_pending_full("ext1", "Cafe", -450, "2026-05-30", None, None, "acc_2"));
    assert_eq!(s.pending_reviews[0].source_monzo_id, "acc_2");
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --no-run`
Expected: FAIL to compile — `AccountLink` unknown, `add_pending_full` arity, field `source_monzo_id` missing.

- [ ] **Step 3: Add `AccountLink` and extend `MonzoState`**

In `src/state.rs` replace the `MonzoState` struct with:

```rust
#[derive(Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonzoState {
    #[serde(default)]
    pub refresh_token: String,
    /// Legacy single Monzo account id. Retained ONLY so old state files still
    /// deserialise; migrated into `accounts` by `State::load`. Do not read directly.
    #[serde(default)]
    pub account_id: String,
    /// Monzo→Actual account links. Empty until `aba accounts` populates it.
    #[serde(default)]
    pub accounts: Vec<AccountLink>,
}

/// One Monzo account mapped to one Actual account.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountLink {
    /// Monzo account id (`acc_…`) — used for fetching.
    pub monzo_id: String,
    /// Monzo display name (suggest/review UX only).
    #[serde(default)]
    pub monzo_name: String,
    /// Actual account UUID — the import target. Empty = unmapped.
    #[serde(default)]
    pub actual_id: String,
    /// Actual account name (suggest/review UX only).
    #[serde(default)]
    pub actual_name: String,
}
```

> The `impl std::fmt::Debug for MonzoState` block stays as-is (it hand-formats to avoid leaking the refresh token); add `.field("accounts", &self.accounts)` to it if it enumerates fields — check and keep it redaction-safe (never print `refresh_token`).

- [ ] **Step 4: Add migration to `State::load`**

Replace the `Ok(s) =>` arm of `State::load` so it migrates after parsing:

```rust
            Ok(s) => {
                let mut state: Self = toml::from_str(&s)?;
                state.migrate_legacy_account();
                Ok(state)
            }
```

Add the private method inside `impl State`:

```rust
    /// One-time migration: an old state file has a single `monzo.account_id`
    /// (the Monzo id) and no `accounts`. Promote it to a single UNMAPPED link —
    /// `actual_id` is left empty because the legacy value was the Monzo side
    /// only; the user supplies the Actual side via `aba accounts`.
    fn migrate_legacy_account(&mut self) {
        if self.monzo.accounts.is_empty() && !self.monzo.account_id.is_empty() {
            self.monzo.accounts.push(AccountLink {
                monzo_id: std::mem::take(&mut self.monzo.account_id),
                ..AccountLink::default()
            });
        }
    }
```

- [ ] **Step 5: Fix `monzo_clone` to carry `accounts`**

Replace `monzo_clone`'s body:

```rust
    /// Clone the Monzo sub-state (for constructing the bank source).
    pub fn monzo_clone(&self) -> MonzoState {
        MonzoState {
            refresh_token: self.monzo.refresh_token.clone(),
            account_id: String::new(),
            accounts: self.monzo.accounts.clone(),
        }
    }
```

- [ ] **Step 6: Thread `source_monzo_id` through pending reviews**

Add the field to `PendingReview` (after `external_id`):

```rust
    /// Monzo account this held txn came from; routes the import once resolved.
    #[serde(default)]
    pub source_monzo_id: String,
```

Add to `ResolvedReview` (find its struct def) a plain field:

```rust
    pub source_monzo_id: String,
```

In `take_resolved`, set it when building `ResolvedReview { … }`:

```rust
                source_monzo_id: pr.source_monzo_id,
```
(Place this line where `pr` fields are moved into the struct; `pr` is owned by the loop so the move is fine.)

Change `add_pending` and `add_pending_full` signatures to accept `source_monzo_id: &str`:

```rust
    pub fn add_pending(
        &mut self,
        external_id: &str,
        raw_merchant: &str,
        amount: i64,
        date: &str,
        source_monzo_id: &str,
    ) -> bool {
        self.add_pending_full(external_id, raw_merchant, amount, date, None, None, source_monzo_id)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_pending_full(
        &mut self,
        external_id: &str,
        raw_merchant: &str,
        amount: i64,
        date: &str,
        suggestion: Option<String>,
        confidence: Option<String>,
        source_monzo_id: &str,
    ) -> bool {
```

In the `PendingReview { … }` literal inside `add_pending_full`, add:

```rust
            source_monzo_id: source_monzo_id.to_string(),
```

- [ ] **Step 7: Fix compiler-flagged construction sites**

Run: `cargo build`
Expected: FAIL at `MonzoState { … }` literals (missing `accounts`) and any `PendingReview { … }` test literals (missing `source_monzo_id`). Fix each MonzoState literal by adding `accounts: Vec::new(),` (or `..Default::default()`), and each PendingReview test literal by adding `source_monzo_id: String::new(),`. Do NOT touch `run.rs` `add_pending*` call sites yet — Task 5 owns those.

> Expect breaks in: `src/state.rs` tests, `src/banks/monzo.rs` (`client` helper + `fetch_with_empty_account_id_errors`). For the monzo `client` helper, set the link now so Task 4 tests have a mapping:
> ```rust
>             crate::state::MonzoState {
>                 refresh_token: "old_rt".into(),
>                 account_id: String::new(),
>                 accounts: vec![crate::state::AccountLink {
>                     monzo_id: "acc_1".into(),
>                     actual_id: "uuid-1".into(),
>                     ..Default::default()
>                 }],
>             },
> ```

(`run.rs` will still fail to compile on its `add_pending*` calls — that's expected and fixed in Task 5. To keep Task 2 independently green, temporarily skip `run.rs` by NOT building the whole crate; instead run the targeted state tests below.)

- [ ] **Step 8: Run the state tests**

Run: `cargo test --test '*' state:: ; cargo test state::tests`
(Practical form: `cargo test -p actual-budget-automation state` — if the crate name differs, use `cargo test state` and read results.)
Expected: the three new tests PASS. (Full-crate `cargo build` stays red until Task 5; that is acceptable mid-plan.)

- [ ] **Step 9: Commit**

```bash
git add src/state.rs src/banks/monzo.rs
git commit -m "feat(state): AccountLink + legacy migration + source_monzo_id on reviews"
```

---

## Task 3: `get_accounts` shim op

**Files:**
- Modify: `src/actual/shim_protocol.rs` (add `GetAccounts` to `ShimRequest`, add `Account` struct, add a serde round-trip test)
- Modify: `src/actual/mod.rs` (add `get_accounts`)
- Modify: `actual-shim/index.js` (add `get_accounts` case)

- [ ] **Step 1: Write failing protocol test**

In `src/actual/shim_protocol.rs` tests module:

```rust
#[test]
fn get_accounts_request_serialises_and_account_deserialises() {
    let req = ShimRequest::GetAccounts;
    let json = serde_json::to_string(&req).unwrap();
    assert_eq!(json, r#"{"op":"get_accounts"}"#);

    let acct: Account =
        serde_json::from_str(r#"{"id":"uuid-1","name":"Monzo Personal"}"#).unwrap();
    assert_eq!(acct, Account { id: "uuid-1".into(), name: "Monzo Personal".into() });
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test get_accounts_request_serialises`
Expected: FAIL — `GetAccounts`/`Account` not found.

- [ ] **Step 3: Add the variant and struct**

In `ShimRequest`, add `GetAccounts,` (alongside `GetPayees`). After the `Payee` struct add:

```rust
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Account {
    pub id: String,
    pub name: String,
}
```

- [ ] **Step 4: Add the client method**

In `src/actual/mod.rs`, after `get_payees`, add (and import `Account` in the `use super::shim_protocol::{…}` group):

```rust
    /// # Errors
    /// Returns an error if the shim fails to spawn, exits non-zero, or returns invalid JSON.
    pub async fn get_accounts(&self) -> anyhow::Result<Vec<Account>> {
        self.call(&ShimRequest::GetAccounts).await
    }
```

- [ ] **Step 5: Add the JS case**

In `actual-shim/index.js`, inside `handle(req)`'s switch, after the `get_payees` case:

```js
    case 'get_accounts': {
      const accounts = await api.getAccounts();
      return accounts.map((a) => ({ id: a.id, name: a.name }));
    }
```

- [ ] **Step 6: Verify test passes**

Run: `cargo test get_accounts_request_serialises`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/actual/shim_protocol.rs src/actual/mod.rs actual-shim/index.js
git commit -m "feat(actual): add read-only get_accounts shim op"
```

---

## Task 4: Per-account Monzo fetch + discovery helpers

**Files:**
- Modify: `src/banks/monzo.rs` (refactor `fetch_since` to loop accounts via a per-account helper; tag `source_monzo_id`; add `list_accounts`/`list_pots`; new structs; update guard + tests)

- [ ] **Step 1: Write failing multi-account routing test**

Add to the monzo `tests` module (mounts two accounts keyed by `account_id`):

```rust
#[tokio::test]
async fn fetch_routes_per_account_and_tags_source() {
    use wiremock::matchers::query_param;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/transactions"))
        .and(query_param("account_id", "acc_1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "transactions": [{
                "id": "a1", "created": "2026-05-30T08:00:00Z", "amount": -100,
                "description": "X", "decline_reason": null, "merchant": null
            }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/transactions"))
        .and(query_param("account_id", "acc_2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "transactions": [{
                "id": "b1", "created": "2026-05-30T09:00:00Z", "amount": -200,
                "description": "Y", "decline_reason": null, "merchant": null
            }]
        })))
        .mount(&server)
        .await;

    let mut m = Monzo::new(
        "cid".into(), "csecret".into(), server.uri(),
        crate::state::MonzoState {
            refresh_token: "rt".into(),
            account_id: String::new(),
            accounts: vec![
                crate::state::AccountLink { monzo_id: "acc_1".into(), actual_id: "u1".into(), ..Default::default() },
                crate::state::AccountLink { monzo_id: "acc_2".into(), actual_id: "u2".into(), ..Default::default() },
            ],
        },
    );
    m.set_access_token_for_test("at_x".into());
    let since = Utc.with_ymd_and_hms(2026, 5, 29, 0, 0, 0).unwrap();
    let txs = m.fetch_since(since).await.unwrap();

    assert_eq!(txs.len(), 2);
    let a = txs.iter().find(|t| t.external_id == "a1").unwrap();
    let b = txs.iter().find(|t| t.external_id == "b1").unwrap();
    assert_eq!(a.source_monzo_id, "acc_1");
    assert_eq!(b.source_monzo_id, "acc_2");
}
```

- [ ] **Step 2: Rename the empty-accounts guard test**

Replace `fetch_with_empty_account_id_errors` with:

```rust
#[tokio::test]
async fn fetch_with_no_accounts_errors() {
    let mut m = Monzo::new(
        "cid".into(), "csecret".into(), "http://unused".into(),
        crate::state::MonzoState {
            refresh_token: "rt".into(),
            account_id: String::new(),
            accounts: Vec::new(),
        },
    );
    m.set_access_token_for_test("at_x".into());
    let since = Utc.with_ymd_and_hms(2026, 5, 29, 0, 0, 0).unwrap();
    let err = m.fetch_since(since).await.unwrap_err();
    assert!(matches!(err, BankError::Other(_)));
}
```

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p actual-budget-automation fetch_routes_per_account fetch_with_no_accounts`
Expected: FAIL (routing not implemented; old guard referenced `account_id`).

- [ ] **Step 4: Refactor `fetch_since` into a per-account loop**

Replace the body of `fetch_since` (the `impl BankSource for Monzo` method) with a thin loop that delegates to a new private helper, and move the existing pagination/cursor walk into that helper. New `fetch_since`:

```rust
    async fn fetch_since(&self, since: DateTime<Utc>) -> Result<Vec<RawTransaction>, BankError> {
        if self.access_token.is_none() {
            return Err(BankError::Other(anyhow::anyhow!("ensure_auth not called")));
        }
        if self.monzo.accounts.is_empty() {
            return Err(BankError::Other(anyhow::anyhow!(
                "no Monzo accounts configured; run `aba accounts` to map Monzo accounts to Actual accounts"
            )));
        }
        let mut out = Vec::new();
        for link in &self.monzo.accounts {
            if link.monzo_id.is_empty() {
                warn!("skipping account link with empty monzo_id");
                continue;
            }
            // Fail-loud: if ANY account errors, propagate — never return a
            // partial Ok that would let last_run advance past dropped data.
            out.extend(self.fetch_account(&link.monzo_id, since).await?);
        }
        Ok(out)
    }
```

Add the private helper in `impl Monzo` (the non-test block at `impl Monzo[0]`, or a new `impl Monzo`), containing the previous walk, parameterised by `monzo_id`, tagging `source_monzo_id`:

```rust
impl Monzo {
    async fn fetch_account(
        &self,
        monzo_id: &str,
        since: DateTime<Utc>,
    ) -> Result<Vec<RawTransaction>, BankError> {
        let token = self
            .access_token
            .as_ref()
            .ok_or_else(|| BankError::Other(anyhow::anyhow!("ensure_auth not called")))?;

        let mut all: Vec<MonzoTx> = Vec::new();
        let mut cursor = since.to_rfc3339();
        for page in 0.. {
            if page >= self.max_pages {
                return Err(BankError::Other(anyhow::anyhow!(
                    "Monzo pagination exceeded {} pages (~{} transactions) for {monzo_id}; aborting to avoid silently truncating — last_run is not advanced, the next run will retry",
                    self.max_pages,
                    self.max_pages * 100
                )));
            }
            let resp = self
                .http
                .get(format!("{}/transactions", self.base_url))
                .bearer_auth(token)
                .query(&[
                    ("account_id", monzo_id),
                    ("since", cursor.as_str()),
                    ("expand[]", "merchant"),
                    ("limit", "100"),
                ])
                .send()
                .await
                .map_err(|e| BankError::Other(e.into()))?;
            if resp.status() == reqwest::StatusCode::UNAUTHORIZED
                || resp.status() == reqwest::StatusCode::FORBIDDEN
            {
                return Err(BankError::AuthExpired("Monzo access token rejected".into()));
            }
            if !resp.status().is_success() {
                let s = resp.status();
                return Err(BankError::Other(anyhow::anyhow!("Monzo transactions error {s}")));
            }
            let list: TxList = resp.json().await.map_err(|e| BankError::Other(e.into()))?;
            let raw_len = list.transactions.len();
            let last_id = list.transactions.last().map(|t| t.id.clone());
            all.extend(list.transactions);
            if raw_len < 100 {
                break;
            }
            match last_id {
                Some(id) => cursor = id,
                None => {
                    return Err(BankError::Other(anyhow::anyhow!(
                        "Monzo returned a full page ({raw_len}) with no last id; cannot advance cursor"
                    )));
                }
            }
        }
        Ok(all
            .into_iter()
            .filter(|t| t.decline_reason.is_none())
            .map(|t| {
                let raw_merchant = match &t.merchant {
                    Some(m) if !m.name.is_empty() => m.name.clone(),
                    _ => t.description.clone(),
                };
                RawTransaction {
                    external_id: t.id,
                    date: t.created.date_naive(),
                    amount_minor: t.amount,
                    raw_merchant,
                    source_monzo_id: monzo_id.to_string(),
                }
            })
            .collect())
    }
}
```

> Remove the now-duplicated old walk from `fetch_since` and delete the old `source_monzo_id: self.monzo.account_id.clone()` line added in Task 1 (it no longer exists once the body is replaced). Ensure `warn` is imported (`use tracing::warn;` — check the existing imports at the top of `monzo.rs`).

- [ ] **Step 5: Add discovery helpers + structs**

Add structs near the other private deserialisation structs:

```rust
#[derive(Debug, Deserialize)]
struct AccountList {
    accounts: Vec<MonzoAccount>,
}
#[derive(Debug, Deserialize)]
struct MonzoAccount {
    id: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    closed: bool,
}
#[derive(Debug, Deserialize)]
struct PotList {
    pots: Vec<MonzoPot>,
}
#[derive(Debug, Deserialize)]
struct MonzoPot {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    deleted: bool,
}
```

Add public discovery methods (return plain tuples so `run.rs` need not see private types):

```rust
impl Monzo {
    /// List open Monzo accounts as (id, name). Requires a prior `ensure_auth`.
    pub async fn list_accounts(&self) -> Result<Vec<(String, String)>, BankError> {
        let token = self
            .access_token
            .as_ref()
            .ok_or_else(|| BankError::Other(anyhow::anyhow!("ensure_auth not called")))?;
        let resp = self
            .http
            .get(format!("{}/accounts", self.base_url))
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| BankError::Other(e.into()))?;
        if !resp.status().is_success() {
            let s = resp.status();
            return Err(BankError::Other(anyhow::anyhow!("Monzo accounts error {s}")));
        }
        let list: AccountList = resp.json().await.map_err(|e| BankError::Other(e.into()))?;
        Ok(list
            .accounts
            .into_iter()
            .filter(|a| !a.closed)
            .map(|a| (a.id, a.description))
            .collect())
    }

    /// List active pots for a current account as (id, name). Informational only.
    pub async fn list_pots(&self, current_account_id: &str) -> Result<Vec<(String, String)>, BankError> {
        let token = self
            .access_token
            .as_ref()
            .ok_or_else(|| BankError::Other(anyhow::anyhow!("ensure_auth not called")))?;
        let resp = self
            .http
            .get(format!("{}/pots", self.base_url))
            .bearer_auth(token)
            .query(&[("current_account_id", current_account_id)])
            .send()
            .await
            .map_err(|e| BankError::Other(e.into()))?;
        if !resp.status().is_success() {
            let s = resp.status();
            return Err(BankError::Other(anyhow::anyhow!("Monzo pots error {s}")));
        }
        let list: PotList = resp.json().await.map_err(|e| BankError::Other(e.into()))?;
        Ok(list
            .pots
            .into_iter()
            .filter(|p| !p.deleted)
            .map(|p| (p.id, p.name))
            .collect())
    }
}
```

- [ ] **Step 6: Update `ensure_auth` (drop the `account_id` backfill)**

In `ensure_auth`, remove the block:

```rust
        if state.monzo.account_id.is_empty() {
            state.monzo.account_id.clone_from(&self.monzo.account_id);
        }
```

(Account discovery is now `aba accounts`' job; keep the `refresh_token` clone-into-state.)

- [ ] **Step 7: Verify**

Run: `cargo test -p actual-budget-automation --lib banks::monzo` (or `cargo test fetch_`)
Expected: the new routing test + renamed guard test + all existing monzo tests PASS. (Full crate still red on `run.rs` until Task 5.)

- [ ] **Step 8: Commit**

```bash
git add src/banks/monzo.rs
git commit -m "feat(monzo): fetch per linked account and tag source; add list_accounts/list_pots"
```

---

## Task 5: Route imports by Actual account in `run.rs`

**Files:**
- Modify: `src/run.rs` (add `actual_id_for`; clone account links; group rows; per-account import; hold unmapped; update `add_pending*` calls and dry-run; tests)

- [ ] **Step 1: Add the routing helper + its test**

Add a free function near the top of `src/run.rs` (and `use crate::state::AccountLink;`):

```rust
/// Resolve the Actual account id for a source Monzo account.
/// Falls back to the sole mapped account when there's exactly one (covers
/// single-account users and legacy held items with an empty source). Returns
/// `None` when the source is unknown and the mapping is ambiguous → caller holds.
fn actual_id_for(source_monzo_id: &str, links: &[AccountLink]) -> Option<String> {
    if let Some(l) = links
        .iter()
        .find(|l| l.monzo_id == source_monzo_id && !l.actual_id.is_empty())
    {
        return Some(l.actual_id.clone());
    }
    let mapped: Vec<&AccountLink> = links.iter().filter(|l| !l.actual_id.is_empty()).collect();
    if let [only] = mapped.as_slice() {
        Some(only.actual_id.clone())
    } else {
        None
    }
}
```

Add to the `run.rs` tests module:

```rust
#[test]
fn actual_id_for_matches_source_then_falls_back_to_sole_mapped() {
    let links = vec![
        AccountLink { monzo_id: "acc_1".into(), actual_id: "u1".into(), ..Default::default() },
        AccountLink { monzo_id: "acc_2".into(), actual_id: "u2".into(), ..Default::default() },
    ];
    assert_eq!(actual_id_for("acc_2", &links).as_deref(), Some("u2"));
    assert_eq!(actual_id_for("acc_unknown", &links), None); // ambiguous → hold

    let single = vec![AccountLink { monzo_id: "acc_1".into(), actual_id: "u1".into(), ..Default::default() }];
    assert_eq!(actual_id_for("", &single).as_deref(), Some("u1")); // legacy fallback
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p actual-budget-automation actual_id_for_matches`
Expected: FAIL — `actual_id_for` not found / `run.rs` not compiling.

- [ ] **Step 3: Clone links + replace the row-assembly preamble**

In `run`, just before section `// 6. Assemble import rows`, add:

```rust
    // Clone links so the routing closure/holds don't fight the borrow checker
    // against `state` mutations below. Cheap: a handful of entries.
    let account_links = state.monzo.accounts.clone();
```

Replace the `push_row` closure with a non-routing row builder + the grouped map:

```rust
    let mut grouped: BTreeMap<String, Vec<ImportTxn>> = BTreeMap::new();
    let mut held = 0usize;

    let make_row = |tx: &RawTransaction, payee_id: &str| ImportTxn {
        date: tx.date.format("%Y-%m-%d").to_string(),
        amount: tx.amount_minor,
        payee: payee_id.to_string(),
        imported_id: tx.external_id.clone(),
    };
```

- [ ] **Step 4: Route the cached-hit and Claude-match loops**

In the cached-hits loop, replace the `(Some(tx), Some(pid)) => push_row(&mut import_rows, tx, pid),` arm with:

```rust
            (Some(tx), Some(pid)) => match actual_id_for(&tx.source_monzo_id, &account_links) {
                Some(aid) => grouped.entry(aid).or_default().push(make_row(tx, pid)),
                None => {
                    warn!(
                        "tx {ext_id} from unmapped Monzo account '{}' — holding (run `aba accounts`)",
                        tx.source_monzo_id
                    );
                    let date = tx.date.format("%Y-%m-%d").to_string();
                    if state.add_pending(&tx.external_id, &tx.raw_merchant, tx.amount_minor, &date, &tx.source_monzo_id) {
                        held += 1;
                    }
                }
            },
```

Update the two existing `add_pending_full(...)` calls in this loop (the cached `None` payee arm) to pass the source as the final arg: append `, &tx.source_monzo_id` before the closing `)`.

In the Claude-matches loop, replace the `(Some(tx), Some(pid)) => { push_row(...) … }` arm so the import push routes; keep the cache-insert logic:

```rust
            (Some(tx), Some(pid)) => match actual_id_for(&tx.source_monzo_id, &account_links) {
                Some(aid) => {
                    grouped.entry(aid).or_default().push(make_row(tx, pid));
                    if !state.payee_cache.contains_key(&tx.raw_merchant) {
                        state.payee_cache.insert(tx.raw_merchant.clone(), payee_name.clone());
                        newly_cached.push((tx.raw_merchant.clone(), payee_name.clone()));
                    }
                }
                None => {
                    warn!(
                        "matched tx {ext_id} from unmapped Monzo account '{}' — holding",
                        tx.source_monzo_id
                    );
                    let date = tx.date.format("%Y-%m-%d").to_string();
                    if state.add_pending(&tx.external_id, &tx.raw_merchant, tx.amount_minor, &date, &tx.source_monzo_id) {
                        held += 1;
                    }
                }
            },
```

Update the Claude-loop `None`-payee defensive `add_pending_full(...)` call to append `, &tx.source_monzo_id`.

- [ ] **Step 5: Route resolved pending reviews**

Replace the resolved-reviews loop body that pushes to `import_rows` with routing by the stored source. Replace:

```rust
        import_rows.push(ImportTxn {
            date: r.date.clone(),
            amount: r.amount,
            payee: pid,
            imported_id: if r.external_id.is_empty() {
                format!("{}:{}", r.raw_merchant, r.date)
            } else {
                r.external_id.clone()
            },
        });
```

with:

```rust
        let row = ImportTxn {
            date: r.date.clone(),
            amount: r.amount,
            payee: pid,
            imported_id: if r.external_id.is_empty() {
                format!("{}:{}", r.raw_merchant, r.date)
            } else {
                r.external_id.clone()
            },
        };
        match actual_id_for(&r.source_monzo_id, &account_links) {
            Some(aid) => grouped.entry(aid).or_default().push(row),
            None => warn!(
                "resolved review '{}' from unmapped Monzo account '{}' — skipping import (run `aba accounts`)",
                r.raw_merchant, r.source_monzo_id
            ),
        }
```

- [ ] **Step 6: Update the proposed/unmatched holds**

The `outcome.proposed` loop's `add_pending_full(...)`: append `, &tx.source_monzo_id`.
The `outcome.unmatched` loop's `add_pending(...)`: append `, &tx.source_monzo_id`.

- [ ] **Step 7: Replace the dry-run report block**

Replace the dry-run `import_rows`-based prints with grouped ones:

```rust
    if dry_run {
        let total: usize = grouped.values().map(Vec::len).sum();
        println!("DRY RUN — no writes to Actual");
        println!("  would import : {total}");
        println!(
            "  would skip   : {} (matched a schedule)",
            fetched_count.saturating_sub(candidates.len())
        );
        println!("  would hold   : {held}");
        for (aid, rows) in &grouped {
            println!("  -> account {aid}: {} txns", rows.len());
            for r in rows {
                println!("    + {} {} -> payee {}", r.date, r.amount, r.payee);
            }
        }
        for (tx, sugg, _) in &outcome.proposed {
            println!("    ? hold {} {} (suggest {})", tx.raw_merchant, tx.amount_minor, sugg);
        }
        for tx in &outcome.unmatched {
            println!("    ? hold {} {}", tx.raw_merchant, tx.amount_minor);
        }
        if !newly_cached.is_empty() {
            println!("  would cache  : {}", newly_cached.len());
            for (raw, payee) in &newly_cached {
                println!("    ~ {raw} -> {payee}");
            }
        }
        persist_rotated_token(cfg, state)?;
        return Ok(());
    }
```

- [ ] **Step 8: Replace the import call with a per-account loop**

Replace the `let summary = if import_rows.is_empty() { … } else { actual.import_transactions(state.monzo.account_id.clone(), import_rows)… };` block with:

```rust
    let mut summary = crate::actual::ImportSummary { added: 0, updated: 0 };
    for (aid, rows) in grouped {
        if rows.is_empty() {
            continue;
        }
        let s = actual
            .import_transactions(aid.clone(), rows)
            .await
            .with_context(|| format!("import_transactions for account {aid}"))?;
        summary.added += s.added;
        summary.updated += s.updated;
    }
```

> `with_context` needs `use anyhow::Context;` — it's already imported (the function uses `.context(...)`). Keep `info!(... summary.added, summary.updated)` as-is.

- [ ] **Step 9: Build, fix any remaining call-site arity, run full suite**

Run: `cargo build`
Expected: PASS. If any `add_pending`/`add_pending_full` call still has the old arity, append the source arg as shown above.

Run: `cargo test`
Expected: PASS (including the new `actual_id_for` test and all prior tasks' tests).

- [ ] **Step 10: Add a routing integration test**

Add to the `run.rs` tests module a test that drives `run` with two mapped accounts and asserts the fake `ActualClient` received one import call per account. Use the existing test doubles in this module (find the existing `run` happy-path test and mirror its harness — reuse its fake `ClaudeMatcher`, `Notifier`, and the `ActualClient` test seam). Assert: a tx with `source_monzo_id = "acc_1"` lands in account `u1`'s rows and `source_monzo_id = "acc_2"` in `u2`'s. If the existing harness imports through a real `ActualClient`, instead assert via dry-run output ordering. Keep it deterministic.

> If the existing `run` tests stub Actual via a trait, extend that stub to record `(account_id, Vec<ImportTxn>)` per call. If they use the concrete `ActualClient` with a mock `node`, prefer asserting on `grouped` behaviour through a smaller unit test of `actual_id_for` (already added) plus a dry-run assertion. Choose the path that matches the existing harness; do not invent a new mocking framework.

Run: `cargo test`
Expected: PASS.

- [ ] **Step 11: Commit**

```bash
git add src/run.rs
git commit -m "feat(run): route imports per Actual account; hold unmapped fail-loud"
```

---

## Task 6: `aba accounts` discovery command

**Files:**
- Modify: `src/config.rs` (`Command` enum)
- Modify: `src/main.rs` (dispatch)
- Modify: `src/run.rs` (add `map_accounts` + `suggest_links`; test `suggest_links`)

- [ ] **Step 1: Write failing `suggest_links` test**

Add to `run.rs` tests:

```rust
#[test]
fn suggest_links_matches_by_name_and_leaves_unmatched_unmapped() {
    let monzo = vec![
        ("acc_1".to_string(), "Personal".to_string()),
        ("acc_2".to_string(), "Joint Account".to_string()),
        ("acc_x".to_string(), "Nothing Like It".to_string()),
    ];
    let actual = vec![
        ("u1".to_string(), "Monzo Personal".to_string()),
        ("u2".to_string(), "Monzo Joint".to_string()),
    ];
    let links = suggest_links(&monzo, &actual, 0.6);
    let p = links.iter().find(|l| l.monzo_id == "acc_1").unwrap();
    assert_eq!(p.actual_id, "u1");
    let j = links.iter().find(|l| l.monzo_id == "acc_2").unwrap();
    assert_eq!(j.actual_id, "u2");
    let x = links.iter().find(|l| l.monzo_id == "acc_x").unwrap();
    assert_eq!(x.actual_id, "", "no confident match → unmapped");
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p actual-budget-automation suggest_links_matches`
Expected: FAIL — `suggest_links` not found.

- [ ] **Step 3: Implement `suggest_links`**

Add to `src/run.rs` (uses `crate::matching::payee_similarity` and `crate::state::AccountLink`):

```rust
/// Auto-suggest Monzo→Actual links by normalised-name similarity. Each Monzo
/// account is linked to its best Actual match above `threshold`; below it the
/// link is left unmapped (`actual_id` empty) for the user to fill in.
fn suggest_links(
    monzo: &[(String, String)],
    actual: &[(String, String)],
    threshold: f64,
) -> Vec<AccountLink> {
    monzo
        .iter()
        .map(|(mid, mname)| {
            let best = actual
                .iter()
                .map(|(aid, aname)| (aid, aname, crate::matching::payee_similarity(mname, aname)))
                .filter(|(_, _, score)| *score >= threshold)
                .max_by(|a, b| a.2.total_cmp(&b.2));
            match best {
                Some((aid, aname, _)) => AccountLink {
                    monzo_id: mid.clone(),
                    monzo_name: mname.clone(),
                    actual_id: aid.clone(),
                    actual_name: aname.clone(),
                },
                None => AccountLink {
                    monzo_id: mid.clone(),
                    monzo_name: mname.clone(),
                    ..AccountLink::default()
                },
            }
        })
        .collect()
}
```

- [ ] **Step 4: Verify the unit test passes**

Run: `cargo test -p actual-budget-automation suggest_links_matches`
Expected: PASS.

- [ ] **Step 5: Implement `map_accounts` (the command body)**

Add to `src/run.rs` (model lock/auth ordering on `run`; reuse `persist_rotated_token`). Add `const ACCOUNT_MATCH_THRESHOLD: f64 = 0.6;` near the other consts:

```rust
/// `aba accounts [--write]`: fetch Monzo accounts/pots + Actual accounts,
/// auto-suggest a mapping, print it, and (with `--write`) persist to state.
/// Run where it can reach Monzo + the Actual shim; review the printed mapping
/// and override by editing `[[monzo.accounts]]` in the state file.
#[allow(clippy::future_not_send)]
pub async fn map_accounts(cfg: &Config, write: bool) -> anyhow::Result<()> {
    let _lock = crate::lock::lock_blocking(&cfg.lock_path)?;
    let mut state = State::load(&cfg.state_path)?;

    let mut bank = crate::banks::monzo::Monzo::new(
        cfg.monzo_client_id.clone(),
        cfg.monzo_client_secret.clone(),
        cfg.monzo_base_url.clone(),
        state.monzo_clone(),
    );
    match bank.ensure_auth(&mut state).await {
        Ok(()) => persist_rotated_token(cfg, &state)?,
        Err(BankError::AuthExpired(m)) => {
            return Err(anyhow::anyhow!("Monzo auth expired: {m}. Run `aba auth monzo` first."));
        }
        Err(BankError::Other(e)) => return Err(e),
    }

    let monzo_accounts = bank.list_accounts().await.map_err(map_bank_err)?;
    let actual = ActualClient::new(
        cfg.shim_dir.clone(),
        true, // read-only: we only call get_accounts
        cfg.actual_server_url.clone(),
        cfg.actual_password.clone(),
        cfg.actual_sync_id.clone(),
    );
    let actual_accounts: Vec<(String, String)> = actual
        .get_accounts()
        .await
        .context("get_accounts")?
        .into_iter()
        .map(|a| (a.id, a.name))
        .collect();

    let links = suggest_links(&monzo_accounts, &actual_accounts, ACCOUNT_MATCH_THRESHOLD);

    println!("Suggested Monzo → Actual account mapping:");
    for l in &links {
        if l.actual_id.is_empty() {
            println!("  {} ({})  →  [UNMAPPED — edit state file]", l.monzo_name, l.monzo_id);
        } else {
            println!("  {} ({})  →  {} ({})", l.monzo_name, l.monzo_id, l.actual_name, l.actual_id);
        }
    }

    // Informational: pots are Phase 3 (transfers), not linked here.
    for (mid, _) in &monzo_accounts {
        match bank.list_pots(mid).await {
            Ok(pots) if !pots.is_empty() => {
                println!("Pots under {mid} (Phase 3 — not linked yet):");
                for (pid, pname) in pots {
                    println!("  {pname} ({pid})");
                }
            }
            Ok(_) => {}
            Err(e) => warn!("could not list pots for {mid}: {}", bank_err_text(&e)),
        }
    }

    if write {
        state.monzo.accounts = links;
        state.save(&cfg.state_path)?;
        println!("\nWrote {} link(s) to {}.", state.monzo.accounts.len(), cfg.state_path.display());
        println!("Review/override any UNMAPPED or wrong rows by editing [[monzo.accounts]] there.");
    } else {
        println!("\n(dry run — re-run with `aba accounts --write` to persist)");
    }
    Ok(())
}

fn map_bank_err(e: BankError) -> anyhow::Error {
    match e {
        BankError::Other(e) => e,
        BankError::AuthExpired(m) => anyhow::anyhow!("Monzo auth expired: {m}"),
    }
}

fn bank_err_text(e: &BankError) -> String {
    match e {
        BankError::Other(e) => e.to_string(),
        BankError::AuthExpired(m) => format!("auth expired: {m}"),
    }
}
```

> Verify imports at the top of `run.rs`: `BankError` (already used), `ActualClient`, `State`, `Config`, `warn`, `Context`. Add any missing `use`.

- [ ] **Step 6: Add the CLI variant**

In `src/config.rs`, add to `Command`:

```rust
    /// Discover Monzo accounts + Actual accounts and auto-suggest a mapping.
    /// Prints the suggestion; `--write` persists it to the state file.
    Accounts {
        /// Persist the suggested mapping into the state file.
        #[arg(long)]
        write: bool,
    },
```

- [ ] **Step 7: Wire dispatch in `main.rs`**

In the `match cli.command` block, add an arm:

```rust
        Command::Accounts { write } => run::map_accounts(&cfg, write).await,
```

- [ ] **Step 8: Build, lint, test**

Run: `cargo build`
Expected: PASS.
Run: `cargo clippy --all-targets -- -D warnings`
Expected: PASS (no pedantic/nursery violations).
Run: `cargo test`
Expected: PASS.

- [ ] **Step 9: Smoke-test the CLI surface**

Run: `cargo run -- accounts --help`
Expected: help text shows the `--write` flag. (Full run needs live Monzo/Actual creds; not part of automated tests.)

- [ ] **Step 10: Commit**

```bash
git add src/config.rs src/main.rs src/run.rs
git commit -m "feat(cli): add `aba accounts` to auto-suggest Monzo→Actual mapping"
```

---

## Task 7: Docs + final verification

**Files:**
- Modify: `docs/RUNBOOK.md` (operator steps)

- [ ] **Step 1: Document the new command + migration**

Add a section to `docs/RUNBOOK.md`:

```markdown
## Account mapping (multi-account)

Transactions are routed to Actual accounts via `[[monzo.accounts]]` links in the
state file (`monzo_id` → `actual_id`). Bootstrap them once:

1. Ensure Monzo is authed (`aba auth monzo`) and a `refresh_token` is in state.
2. Run `aba accounts` to preview the auto-suggested mapping (Monzo names matched
   to Actual account names). Pots are listed for awareness but are NOT linked
   (they become transfers in a later phase).
3. Run `aba accounts --write` to persist it.
4. Review the state file's `[[monzo.accounts]]` entries; fix any `actual_id` that
   is empty (UNMAPPED) or wrong by editing it directly.

**Migration:** an old state file with a single `[monzo] account_id` is auto-promoted
on first load to one UNMAPPED `[[monzo.accounts]]` link (the old value is the Monzo
id; the Actual side must be supplied via `aba accounts`). Until mapped, transactions
from that account are HELD (never imported to a wrong/blank account) and surfaced in
the run summary.
```

- [ ] **Step 2: Full verification gate**

Run: `cargo build && cargo test && cargo clippy --all-targets -- -D warnings`
Expected: all PASS, zero warnings.

- [ ] **Step 3: Commit**

```bash
git add docs/RUNBOOK.md
git commit -m "docs(runbook): document aba accounts + multi-account migration"
```

---

## Self-review notes (verified against the spec)

- **`account_id` split** → Tasks 1, 2, 4, 5 (fetch uses `monzo_id`; import uses `actual_id`).
- **Per-account fetch + tagging** → Task 4 (`fetch_account` + `source_monzo_id`).
- **Route imports / hold unmapped fail-loud** → Task 5 (`actual_id_for`, `grouped`, per-account import, holds).
- **Held-then-resolved routing** → Task 2 + Task 5 (source on `PendingReview`/`ResolvedReview`; resolved-review routing).
- **`get_accounts` shim op** → Task 3.
- **`aba accounts` auto-suggest + review/override** → Task 6 (`suggest_links`, `map_accounts`, `--write`).
- **Pot listing (informational)** → Task 6 (`list_pots`, printed, flagged Phase 3).
- **Legacy state migration** → Task 2 (`migrate_legacy_account`).
- **Single-account fallback** → Task 5 (`actual_id_for` sole-mapped branch).
- **Type consistency:** `actual_id_for(&str, &[AccountLink]) -> Option<String>`, `suggest_links(&[(String,String)], &[(String,String)], f64) -> Vec<AccountLink>`, `add_pending*( …, source_monzo_id: &str)`, `ImportSummary { added: usize, updated: usize }` — consistent across tasks.
- **Open items deferred to execution:** exact run-summary surfacing of held-unmapped counts (Task 5 logs + existing held count covers the minimum); the `0.6` threshold is a starting value to tune.
```
