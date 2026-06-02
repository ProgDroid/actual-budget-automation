# Monzo Transaction Pagination Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `Monzo::fetch_since` walk every page of the requested window via Monzo cursor pagination, replacing the single-request + `warn!`-at-100 behaviour, with a loud runaway guard that never silently truncates.

**Architecture:** A sequential loop over `GET /transactions` (`limit=100`). The first page uses `since=<RFC3339 timestamp>`; each subsequent page uses `since=<last raw transaction id>` (Monzo cursor). The page-size check and the cursor both use the *raw* response count/id (before decline filtering), so filtered declines can't falsely end pagination. A `MAX_PAGES` guard returns `BankError::Other` if exceeded so `run.rs` never advances `last_run` on a truncated fetch.

**Tech Stack:** Rust, `async_trait`, `reqwest`, `wiremock` (test mocks), `tokio::test`.

---

## File Structure

- **Modify:** `src/banks/monzo.rs` — the only file touched. Changes are confined to:
  - the `Monzo` struct (add `max_pages` field),
  - `Monzo::new` (initialise `max_pages`),
  - the `#[cfg(test)] impl Monzo` block (add `set_max_pages_for_test`),
  - the `fetch_since` method (replace single request with the loop),
  - the top-of-file imports (remove the now-unused `use tracing::warn;`),
  - the `tests` module (add three tests).

No other files change. `run.rs`, the `BankSource` trait, and the import path are untouched.

## Reference: project harness gotchas

- `aba` is a **binary** crate — run `cargo test` / `cargo test monzo`, **never** `cargo test --lib` (it errors "no library targets found").
- Treat any `<new-diagnostics>` (rust-analyzer) as advisory only; trust `cargo build` / `cargo clippy --all-targets -- -D warnings` / `cargo test` output instead.
- The clippy gate is default lints only; `unused_imports` is default-on, so the `use tracing::warn;` removal in Task 1 is required (leaving it fails `-D warnings`).

---

## Task 1: Cursor pagination loop (core behaviour)

**Files:**
- Modify: `src/banks/monzo.rs` (struct `Monzo` ~lines 12-19; `Monzo::new` ~lines 51-67; `#[cfg(test)] impl Monzo` ~lines 69-74; `fetch_since` ~lines 136-197; imports ~line 7)
- Test: `src/banks/monzo.rs` (`mod tests`)

- [ ] **Step 1: Write the failing test**

Add this test inside `mod tests` (after `fetch_maps_merchant_with_description_fallback_and_skips_declined`). It mounts two mocks differentiated by the `since` query param: page 1 (matched by the RFC3339 window-start timestamp) returns 100 rows whose last id is `tx_99`; page 2 (matched by `since=tx_99`) returns 2 rows. A correct implementation fetches both pages (102 total).

```rust
use wiremock::matchers::query_param;

#[tokio::test]
async fn fetch_paginates_via_last_id_cursor() {
    let server = MockServer::start().await;
    let since = Utc.with_ymd_and_hms(2026, 5, 29, 0, 0, 0).unwrap();

    // Page 1: 100 rows, last id = tx_99. Matched by the timestamp cursor.
    let mut page1 = Vec::new();
    for i in 0..100 {
        page1.push(serde_json::json!({
            "id": format!("tx_{i}"),
            "created": "2026-05-30T08:15:00Z",
            "amount": -100,
            "description": "X",
            "decline_reason": null,
            "merchant": null
        }));
    }
    Mock::given(method("GET"))
        .and(path("/transactions"))
        .and(query_param("since", since.to_rfc3339()))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "transactions": page1 })),
        )
        .mount(&server)
        .await;

    // Page 2: cursor advanced to the last id of page 1. Short page ends the walk.
    Mock::given(method("GET"))
        .and(path("/transactions"))
        .and(query_param("since", "tx_99"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "transactions": [
                { "id": "tx_100", "created": "2026-05-31T09:00:00Z", "amount": -200,
                  "description": "Y", "decline_reason": null, "merchant": null },
                { "id": "tx_101", "created": "2026-05-31T10:00:00Z", "amount": -300,
                  "description": "Z", "decline_reason": null, "merchant": null }
            ]
        })))
        .mount(&server)
        .await;

    let mut m = client(server.uri());
    m.set_access_token_for_test("at_x".into());
    let txs = m.fetch_since(since).await.unwrap();

    assert_eq!(txs.len(), 102, "both pages should be fetched and concatenated");
    assert_eq!(txs[0].external_id, "tx_0");
    assert_eq!(txs[101].external_id, "tx_101");
    assert_eq!(txs[101].amount_minor, -300);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test fetch_paginates_via_last_id_cursor`
Expected: FAIL. With the current single-request implementation only page 1 is fetched, so `txs.len()` is 100, not 102 (assertion failure). (The `set_max_pages_for_test` call in later tests is not used here, so this test compiles against the current API too — it fails on the length assertion, which is the desired red state.)

- [ ] **Step 3: Write minimal implementation**

3a. Remove the now-unused warn import near the top of the file:

```rust
// DELETE this line (currently ~line 7):
use tracing::warn;
```

3b. Add the `max_pages` field to the `Monzo` struct:

```rust
pub struct Monzo {
    http: reqwest::Client,
    client_id: String,
    client_secret: String,
    base_url: String,
    monzo: MonzoState,
    access_token: Option<String>,
    max_pages: usize,
}
```

3c. Add the module-level constant just above `impl Monzo` (the first one, ~line 51):

```rust
/// Runaway guard for `fetch_since` pagination. At `limit=100` per page this is
/// ~10,000 transactions — far above any realistic personal-account window. If a
/// fetch would exceed it we abort loudly rather than silently truncate.
const MAX_PAGES: usize = 100;
```

3d. Initialise `max_pages` in `Monzo::new`:

```rust
    pub fn new(
        client_id: String,
        client_secret: String,
        base_url: String,
        monzo: MonzoState,
    ) -> Self {
        Self {
            http: reqwest::Client::new(),
            client_id,
            client_secret,
            base_url,
            monzo,
            access_token: None,
            max_pages: MAX_PAGES,
        }
    }
```

3e. Add a test-only override in the existing `#[cfg(test)] impl Monzo` block (next to `set_access_token_for_test`):

```rust
    #[cfg(test)]
    pub fn set_max_pages_for_test(&mut self, max_pages: usize) {
        self.max_pages = max_pages;
    }
```

3f. Replace the body of `fetch_since` from the `// TODO(phase2)` comment through the end of the method (i.e. everything after the `account_id` empty check) with the loop below. The preamble (the `token` lookup and the `account_id.is_empty()` check) stays exactly as it is.

```rust
        // Walk every page via Monzo's cursor: the first page uses the window-start
        // timestamp; each later page uses the previous page's last raw id. The
        // page-size check and the cursor both use the RAW count/id (before decline
        // filtering) so filtered-out declines can never falsely end the walk.
        let mut all: Vec<MonzoTx> = Vec::new();
        let mut cursor = since.to_rfc3339();
        for page in 0.. {
            if page >= self.max_pages {
                return Err(BankError::Other(anyhow::anyhow!(
                    "Monzo pagination exceeded {} pages (~{} transactions); aborting to avoid silently truncating — last_run is not advanced, the next run will retry",
                    self.max_pages,
                    self.max_pages * 100
                )));
            }

            let resp = self
                .http
                .get(format!("{}/transactions", self.base_url))
                .bearer_auth(token)
                .query(&[
                    ("account_id", self.monzo.account_id.as_str()),
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
                return Err(BankError::Other(anyhow::anyhow!(
                    "Monzo transactions error {s}"
                )));
            }

            let list: TxList = resp.json().await.map_err(|e| BankError::Other(e.into()))?;
            let raw_len = list.transactions.len();
            let last_id = list.transactions.last().map(|t| t.id.clone());
            all.extend(list.transactions);

            if raw_len < 100 {
                break; // final, partial page
            }
            match last_id {
                Some(id) => cursor = id,
                None => break, // unreachable: raw_len == 100 implies a last row
            }
        }

        let out = all
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
                }
            })
            .collect();
        Ok(out)
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test fetch_paginates_via_last_id_cursor`
Expected: PASS.

- [ ] **Step 5: Run the existing module tests to confirm no regression**

Run: `cargo test monzo`
Expected: PASS — including `fetch_maps_merchant_with_description_fallback_and_skips_declined` (3 rows < 100 → single page), `fetch_with_empty_account_id_errors`, `fetch_403_is_auth_expired`, and the refresh-token tests.

- [ ] **Step 6: Commit**

```bash
git add src/banks/monzo.rs
git commit -m "feat(monzo): paginate fetch_since via last-id cursor"
```

---

## Task 2: Decline-on-boundary guard test

Proves the page-size check uses the **raw** count, not the post-decline-filter count: a full page of 100 that contains declined rows must still trigger a second fetch.

**Files:**
- Test: `src/banks/monzo.rs` (`mod tests`)

- [ ] **Step 1: Write the test**

```rust
#[tokio::test]
async fn fetch_pages_on_raw_count_not_filtered_count() {
    let server = MockServer::start().await;
    let since = Utc.with_ymd_and_hms(2026, 5, 29, 0, 0, 0).unwrap();

    // Page 1: 100 raw rows, 40 of them declined (mapped output would be 60),
    // last id = tx_99. The raw count is 100 so a second page MUST be requested.
    let mut page1 = Vec::new();
    for i in 0..100 {
        let declined = i % 10 < 4; // 40 declined
        page1.push(serde_json::json!({
            "id": format!("tx_{i}"),
            "created": "2026-05-30T08:15:00Z",
            "amount": -100,
            "description": "X",
            "decline_reason": if declined { serde_json::json!("INSUFFICIENT_FUNDS") } else { serde_json::json!(null) },
            "merchant": null
        }));
    }
    Mock::given(method("GET"))
        .and(path("/transactions"))
        .and(query_param("since", since.to_rfc3339()))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "transactions": page1 })),
        )
        .mount(&server)
        .await;

    // Page 2 (cursor = tx_99): one non-declined row, then the walk ends.
    Mock::given(method("GET"))
        .and(path("/transactions"))
        .and(query_param("since", "tx_99"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "transactions": [
                { "id": "tx_100", "created": "2026-05-31T09:00:00Z", "amount": -200,
                  "description": "Y", "decline_reason": null, "merchant": null }
            ]
        })))
        .mount(&server)
        .await;

    let mut m = client(server.uri());
    m.set_access_token_for_test("at_x".into());
    let txs = m.fetch_since(since).await.unwrap();

    // 60 non-declined from page 1 + 1 from page 2 = 61.
    assert_eq!(txs.len(), 61);
    assert_eq!(txs[60].external_id, "tx_100");
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test fetch_pages_on_raw_count_not_filtered_count`
Expected: PASS. (The behaviour was implemented in Task 1; this test guards against a future regression where someone moves decline filtering before the page-size check. If page 2 were not requested, `query_param("since", "tx_99")` would go unmatched, the mock would return 404, and `fetch_since` would error — failing the `.unwrap()`.)

- [ ] **Step 3: Commit**

```bash
git add src/banks/monzo.rs
git commit -m "test(monzo): pagination continues on raw count despite declines"
```

---

## Task 3: Runaway cap guard test

Proves that when pages never shrink below 100, `fetch_since` aborts with `BankError::Other` after `max_pages` requests instead of looping forever. Uses `set_max_pages_for_test(3)` to keep it to 3 round-trips.

**Files:**
- Test: `src/banks/monzo.rs` (`mod tests`)

- [ ] **Step 1: Write the test**

```rust
#[tokio::test]
async fn fetch_aborts_when_pagination_exceeds_cap() {
    let server = MockServer::start().await;
    let since = Utc.with_ymd_and_hms(2026, 5, 29, 0, 0, 0).unwrap();

    // Every request returns a full page of 100, so the walk never reaches a
    // short final page. With no `since` matcher this responds to all cursors.
    let mut full = Vec::new();
    for i in 0..100 {
        full.push(serde_json::json!({
            "id": format!("tx_{i}"),
            "created": "2026-05-30T08:15:00Z",
            "amount": -100,
            "description": "X",
            "decline_reason": null,
            "merchant": null
        }));
    }
    Mock::given(method("GET"))
        .and(path("/transactions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "transactions": full })),
        )
        .mount(&server)
        .await;

    let mut m = client(server.uri());
    m.set_access_token_for_test("at_x".into());
    m.set_max_pages_for_test(3);
    let err = m.fetch_since(since).await.unwrap_err();

    assert!(matches!(err, BankError::Other(_)));
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test fetch_aborts_when_pagination_exceeds_cap`
Expected: PASS. The loop fetches 3 full pages (page indices 0, 1, 2), then on index 3 the `page >= self.max_pages` guard returns `BankError::Other`.

- [ ] **Step 3: Commit**

```bash
git add src/banks/monzo.rs
git commit -m "test(monzo): runaway pagination aborts at the page cap"
```

---

## Task 4: Full verification gate

**Files:** none (verification only).

- [ ] **Step 1: Clippy gate (the CI gate)**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: no warnings, no errors. In particular, confirm there is no `unused_imports` warning for `tracing::warn` (it was removed in Task 1).

- [ ] **Step 2: Full test suite**

Run: `cargo test`
Expected: all tests pass, including the three new pagination tests and every pre-existing test.

- [ ] **Step 3: Build (release-shaped) sanity**

Run: `cargo build`
Expected: clean build.

- [ ] **Step 4: Final commit (if anything was adjusted during verification)**

Only if Steps 1-3 required a fix:

```bash
git add -A
git commit -m "chore(monzo): clippy/test cleanup for pagination"
```

---

## Self-Review

**Spec coverage:**
- Walk all pages for the window → Task 1 loop. ✓
- Cursor = last raw id; first page = timestamp → Task 1 (`cursor` init + advance). ✓
- Page-size check & cursor on raw count → Task 1 (`raw_len`, `last_id` before filter) + Task 2 guard test. ✓
- Stop at `< 100` → Task 1 `if raw_len < 100 { break }`. ✓
- Runaway guard → loud `BankError::Other` → Task 1 guard + Task 3 test. ✓
- Preserve decline filter / merchant fallback / error mapping / `RawTransaction` shape → Task 1 (map closure moved verbatim, per-page error mapping unchanged) + Task 1 Step 5 regression run. ✓
- Remove old `warn!`-at-100 and `TODO(phase2)` → Task 1 Step 3a/3f. ✓
- Tests: two-page walk, single-page regression, decline boundary, runaway cap → Tasks 1, 2, 3 (single-page regression covered by the pre-existing `fetch_maps_...` test, re-run in Task 1 Step 5). ✓

**Placeholder scan:** No TBD/TODO/"handle edge cases" left; the removed `TODO(phase2)` is the only TODO and it is deleted in Task 1. ✓

**Type consistency:** `max_pages: usize`, `MAX_PAGES: usize`, `set_max_pages_for_test(usize)`, `set_access_token_for_test` (pre-existing), `BankError::Other` / `BankError::AuthExpired`, `RawTransaction { external_id, date, amount_minor, raw_merchant }`, `MonzoTx`/`TxList` — all consistent across tasks. ✓
