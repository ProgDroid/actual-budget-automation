//! Monzo bank source.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use tracing::warn;

use crate::banks::{BankError, BankSource, RawTransaction};
use crate::state::{MonzoState, State};

// `monzo` holds this bank's persisted MonzoState; the name mirrors the domain and
// reads clearly at call sites, so the struct-name overlap is intentional.
#[allow(clippy::struct_field_names)]
pub struct Monzo {
    http: reqwest::Client,
    client_id: String,
    client_secret: String,
    base_url: String,
    monzo: MonzoState,
    access_token: Option<String>,
    max_pages: usize,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
}

#[derive(Debug, Deserialize)]
struct TxList {
    transactions: Vec<MonzoTx>,
}

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
}

#[derive(Debug, Deserialize)]
struct Merchant {
    #[serde(default)]
    name: String,
}

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

/// Runaway guard for `fetch_since` pagination. At `limit=100` per page this is
/// ~10,000 transactions — far above any realistic personal-account window. If a
/// fetch would exceed it we abort loudly rather than silently truncate.
const MAX_PAGES: usize = 100;

impl Monzo {
    pub fn new(
        client_id: String,
        client_secret: String,
        base_url: String,
        monzo: MonzoState,
    ) -> Self {
        Self {
            http: crate::http::client(std::time::Duration::from_secs(30)),
            client_id,
            client_secret,
            base_url,
            monzo,
            access_token: None,
            max_pages: MAX_PAGES,
        }
    }
}

impl Monzo {
    #[cfg(test)]
    pub fn set_access_token_for_test(&mut self, token: String) {
        self.access_token = Some(token);
    }

    #[cfg(test)]
    pub const fn set_max_pages_for_test(&mut self, max_pages: usize) {
        self.max_pages = max_pages;
    }
}

impl Monzo {
    /// Fetch all transactions for a single Monzo account since `since`, walking
    /// pagination via cursor. Fails loud: if the fetch errors the whole
    /// `fetch_since` errors (no partial Ok).
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
    pub async fn list_pots(
        &self,
        current_account_id: &str,
    ) -> Result<Vec<(String, String)>, BankError> {
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

#[async_trait]
impl BankSource for Monzo {
    fn name(&self) -> &'static str {
        "monzo"
    }

    /// # Errors
    /// Returns [`BankError::AuthExpired`] when there is no refresh token in
    /// state or Monzo rejects the refresh with 401/403 (a 90-day human re-auth
    /// is required). Returns [`BankError::Other`] for transport failures, other
    /// non-success statuses, or a response body that fails to deserialize.
    async fn ensure_auth(&mut self, state: &mut State) -> Result<(), BankError> {
        if self.monzo.refresh_token.is_empty() {
            return Err(BankError::AuthExpired(
                "no Monzo refresh token in state".into(),
            ));
        }
        let resp = self
            .http
            .post(format!("{}/oauth2/token", self.base_url))
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("refresh_token", self.monzo.refresh_token.as_str()),
            ])
            .send()
            .await
            .map_err(|e| BankError::Other(e.into()))?;

        if resp.status() == reqwest::StatusCode::UNAUTHORIZED
            || resp.status() == reqwest::StatusCode::FORBIDDEN
        {
            return Err(BankError::AuthExpired(format!(
                "Monzo refresh rejected ({})",
                resp.status()
            )));
        }
        if !resp.status().is_success() {
            let s = resp.status();
            return Err(BankError::Other(anyhow::anyhow!("Monzo token error {s}")));
        }

        let token: TokenResponse = resp.json().await.map_err(|e| BankError::Other(e.into()))?;
        self.access_token = Some(token.access_token);
        // Persist the rotated refresh token immediately.
        self.monzo.refresh_token = token.refresh_token;
        state
            .monzo
            .refresh_token
            .clone_from(&self.monzo.refresh_token);
        Ok(())
    }

    /// # Errors
    /// Returns [`BankError::AuthExpired`] when Monzo rejects the access token
    /// with 401/403 (refresh/re-auth required). Returns [`BankError::Other`]
    /// when `ensure_auth` has not been called, no accounts are configured,
    /// the transport fails, the status is otherwise non-success, or the body
    /// fails to deserialize.
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::State;
    use chrono::TimeZone;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(base: String) -> Monzo {
        Monzo::new(
            "cid".into(),
            "csecret".into(),
            base,
            crate::state::MonzoState {
                refresh_token: "old_rt".into(),
                account_id: String::new(),
                accounts: vec![crate::state::AccountLink {
                    monzo_id: "acc_1".into(),
                    actual_id: "uuid-1".into(),
                    ..Default::default()
                }],
            },
        )
    }

    #[tokio::test]
    async fn refresh_rotates_and_persists_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth2/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "at_new", "refresh_token": "rt_new",
                "token_type": "Bearer", "expires_in": 21600
            })))
            .mount(&server)
            .await;

        let mut m = client(server.uri());
        let mut state = State::default();
        m.ensure_auth(&mut state).await.unwrap();
        assert_eq!(state.monzo.refresh_token, "rt_new");
    }

    #[tokio::test]
    async fn refresh_failure_is_auth_expired() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth2/token"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "code": "unauthorized.bad_refresh_token"
            })))
            .mount(&server)
            .await;

        let mut m = client(server.uri());
        let mut state = State::default();
        let err = m.ensure_auth(&mut state).await.unwrap_err();
        assert!(matches!(err, BankError::AuthExpired(_)));
    }

    #[tokio::test]
    async fn fetch_maps_merchant_with_description_fallback_and_skips_declined() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/transactions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "transactions": [
                    {
                        "id": "tx_1", "created": "2026-05-30T08:15:00Z", "amount": -450,
                        "description": "SQ *SOME CAFE", "decline_reason": null,
                        "merchant": { "name": "Some Cafe" }
                    },
                    {
                        "id": "tx_2", "created": "2026-05-31T09:00:00Z", "amount": -1999,
                        "description": "AMZNMKTPLACE", "decline_reason": null,
                        "merchant": null
                    },
                    {
                        "id": "tx_3", "created": "2026-05-31T10:00:00Z", "amount": -100,
                        "description": "DECLINED THING", "decline_reason": "INSUFFICIENT_FUNDS",
                        "merchant": null
                    }
                ]
            })))
            .mount(&server)
            .await;

        let mut m = client(server.uri());
        m.set_access_token_for_test("at_x".into());
        let since = Utc.with_ymd_and_hms(2026, 5, 29, 0, 0, 0).unwrap();
        let txs = m.fetch_since(since).await.unwrap();

        assert_eq!(txs.len(), 2);
        assert_eq!(txs[0].external_id, "tx_1");
        assert_eq!(txs[0].raw_merchant, "Some Cafe");
        assert_eq!(txs[1].raw_merchant, "AMZNMKTPLACE");
        assert_eq!(txs[1].amount_minor, -1999);
    }

    #[tokio::test]
    async fn fetch_with_no_accounts_errors() {
        let mut m = Monzo::new(
            "cid".into(),
            "csecret".into(),
            "http://unused".into(),
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

    #[tokio::test]
    async fn fetch_403_is_auth_expired() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/transactions"))
            .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
                "code": "forbidden.insufficient_permissions"
            })))
            .mount(&server)
            .await;

        let mut m = client(server.uri());
        m.set_access_token_for_test("at_x".into());
        let since = Utc.with_ymd_and_hms(2026, 5, 29, 0, 0, 0).unwrap();
        let err = m.fetch_since(since).await.unwrap_err();
        assert!(matches!(err, BankError::AuthExpired(_)));
    }

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
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "transactions": page1 })),
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

        assert_eq!(
            txs.len(),
            102,
            "both pages should be fetched and concatenated"
        );
        assert_eq!(txs[0].external_id, "tx_0");
        assert_eq!(txs[101].external_id, "tx_101");
        assert_eq!(txs[101].amount_minor, -300);
    }

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
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "transactions": page1 })),
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
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "transactions": full })),
            )
            .mount(&server)
            .await;

        let mut m = client(server.uri());
        m.set_access_token_for_test("at_x".into());
        m.set_max_pages_for_test(3);
        let err = m.fetch_since(since).await.unwrap_err();

        assert!(matches!(err, BankError::Other(_)));
        let msg = format!("{err}");
        assert!(
            msg.contains("pages"),
            "cap error should mention pages, got: {msg}"
        );
    }

    #[tokio::test]
    async fn fetch_cursor_uses_raw_last_id_when_last_row_declined() {
        let server = MockServer::start().await;
        let since = Utc.with_ymd_and_hms(2026, 5, 29, 0, 0, 0).unwrap();

        // Page 1: 100 raw rows. The LAST raw row (tx_99) IS declined; tx_98 is
        // not. The cursor MUST advance to the last RAW id (tx_99), not the last
        // non-declined id (tx_98). If the code regressed to using the last
        // non-declined id, the page-2 mock (keyed on tx_99) would not match,
        // wiremock would 404, and fetch_since would error → .unwrap() panics.
        let mut page1 = Vec::new();
        for i in 0..100 {
            let declined = i == 99; // only the last raw row is declined
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
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "transactions": page1 })),
            )
            .mount(&server)
            .await;

        // Page 2 keyed on the RAW last id (tx_99): short final page, one row.
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

        assert_eq!(txs.last().unwrap().external_id, "tx_100");
    }

    #[tokio::test]
    async fn fetch_terminates_on_empty_final_page() {
        let server = MockServer::start().await;
        let since = Utc.with_ymd_and_hms(2026, 5, 29, 0, 0, 0).unwrap();

        // Page 1: exactly 100 rows (last id tx_99) → a second page is requested.
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
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "transactions": page1 })),
            )
            .mount(&server)
            .await;

        // Page 2 (cursor = tx_99): empty page ends the walk via raw_len < 100.
        Mock::given(method("GET"))
            .and(path("/transactions"))
            .and(query_param("since", "tx_99"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "transactions": []
            })))
            .mount(&server)
            .await;

        let mut m = client(server.uri());
        m.set_access_token_for_test("at_x".into());
        let txs = m.fetch_since(since).await.unwrap();

        assert_eq!(txs.len(), 100);
    }

    #[tokio::test]
    async fn fetch_routes_per_account_and_tags_source() {
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
            "cid".into(),
            "csecret".into(),
            server.uri(),
            crate::state::MonzoState {
                refresh_token: "rt".into(),
                account_id: String::new(),
                accounts: vec![
                    crate::state::AccountLink {
                        monzo_id: "acc_1".into(),
                        actual_id: "u1".into(),
                        ..Default::default()
                    },
                    crate::state::AccountLink {
                        monzo_id: "acc_2".into(),
                        actual_id: "u2".into(),
                        ..Default::default()
                    },
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

    #[tokio::test]
    async fn list_accounts_filters_closed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/accounts"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "accounts": [
                    {"id": "acc_1", "description": "Personal", "closed": false},
                    {"id": "acc_old", "description": "Old", "closed": true}
                ]
            })))
            .mount(&server)
            .await;
        let mut m = client(server.uri());
        m.set_access_token_for_test("at_x".into());
        let accts = m.list_accounts().await.unwrap();
        assert_eq!(accts, vec![("acc_1".to_string(), "Personal".to_string())]);
    }

    #[tokio::test]
    async fn list_pots_filters_deleted() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/pots"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "pots": [
                    {"id": "pot_1", "name": "Savings", "deleted": false},
                    {"id": "pot_x", "name": "Gone", "deleted": true}
                ]
            })))
            .mount(&server)
            .await;
        let mut m = client(server.uri());
        m.set_access_token_for_test("at_x".into());
        let pots = m.list_pots("acc_1").await.unwrap();
        assert_eq!(pots, vec![("pot_1".to_string(), "Savings".to_string())]);
    }
}
