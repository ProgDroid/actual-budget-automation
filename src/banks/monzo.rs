//! Monzo bank source.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;

use tracing::warn;

use crate::banks::{BankError, BankSource, RawTransaction};
use crate::state::{MonzoState, State};

pub struct Monzo {
    http: reqwest::Client,
    client_id: String,
    client_secret: String,
    base_url: String,
    monzo: MonzoState,
    access_token: Option<String>,
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

impl Monzo {
    pub fn new(client_id: String, client_secret: String, base_url: String, monzo: MonzoState) -> Self {
        Self {
            http: reqwest::Client::new(),
            client_id,
            client_secret,
            base_url,
            monzo,
            access_token: None,
        }
    }
}

impl Monzo {
    #[cfg(test)]
    pub fn set_access_token_for_test(&mut self, token: String) {
        self.access_token = Some(token);
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
            return Err(BankError::AuthExpired("no Monzo refresh token in state".into()));
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
        state.monzo.refresh_token = self.monzo.refresh_token.clone();
        if state.monzo.account_id.is_empty() {
            state.monzo.account_id = self.monzo.account_id.clone();
        }
        Ok(())
    }

    /// # Errors
    /// Returns [`BankError::AuthExpired`] when Monzo rejects the access token
    /// with 401/403 (refresh/re-auth required). Returns [`BankError::Other`]
    /// when `ensure_auth` has not been called, the account id is unconfigured,
    /// the transport fails, the status is otherwise non-success, or the body
    /// fails to deserialize.
    async fn fetch_since(&self, since: DateTime<Utc>) -> Result<Vec<RawTransaction>, BankError> {
        let token = self
            .access_token
            .as_ref()
            .ok_or_else(|| BankError::Other(anyhow::anyhow!("ensure_auth not called")))?;
        if self.monzo.account_id.is_empty() {
            return Err(BankError::Other(anyhow::anyhow!(
                "Monzo account_id not configured; set [monzo].account_id in the state file"
            )));
        }
        // TODO(phase2): paginate via `before` cursor when 100 are returned.
        let resp = self
            .http
            .get(format!("{}/transactions", self.base_url))
            .bearer_auth(token)
            .query(&[
                ("account_id", self.monzo.account_id.as_str()),
                ("since", &since.to_rfc3339()),
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
        if list.transactions.len() == 100 {
            warn!("Monzo returned 100 transactions (page limit); pagination not implemented — some transactions may be missed this run");
        }
        let out = list
            .transactions
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::State;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(base: String) -> Monzo {
        Monzo::new(
            "cid".into(), "csecret".into(), base,
            crate::state::MonzoState { refresh_token: "old_rt".into(), account_id: "acc_1".into() },
        )
    }

    #[tokio::test]
    async fn refresh_rotates_and_persists_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/oauth2/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "at_new", "refresh_token": "rt_new",
                "token_type": "Bearer", "expires_in": 21600
            })))
            .mount(&server).await;

        let mut m = client(server.uri());
        let mut state = State::default();
        m.ensure_auth(&mut state).await.unwrap();
        assert_eq!(state.monzo.refresh_token, "rt_new");
    }

    #[tokio::test]
    async fn refresh_failure_is_auth_expired() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/oauth2/token"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "code": "unauthorized.bad_refresh_token"
            })))
            .mount(&server).await;

        let mut m = client(server.uri());
        let mut state = State::default();
        let err = m.ensure_auth(&mut state).await.unwrap_err();
        assert!(matches!(err, BankError::AuthExpired(_)));
    }

    use chrono::TimeZone;

    #[tokio::test]
    async fn fetch_maps_merchant_with_description_fallback_and_skips_declined() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/transactions"))
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
            .mount(&server).await;

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
    async fn fetch_with_empty_account_id_errors() {
        let mut m = Monzo::new(
            "cid".into(),
            "csecret".into(),
            "http://unused".into(),
            crate::state::MonzoState { refresh_token: "rt".into(), account_id: String::new() },
        );
        m.set_access_token_for_test("at_x".into());
        let since = Utc.with_ymd_and_hms(2026, 5, 29, 0, 0, 0).unwrap();
        let err = m.fetch_since(since).await.unwrap_err();
        assert!(matches!(err, BankError::Other(_)));
    }

    #[tokio::test]
    async fn fetch_403_is_auth_expired() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/transactions"))
            .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
                "code": "forbidden.insufficient_permissions"
            })))
            .mount(&server).await;

        let mut m = client(server.uri());
        m.set_access_token_for_test("at_x".into());
        let since = Utc.with_ymd_and_hms(2026, 5, 29, 0, 0, 0).unwrap();
        let err = m.fetch_since(since).await.unwrap_err();
        assert!(matches!(err, BankError::AuthExpired(_)));
    }
}
