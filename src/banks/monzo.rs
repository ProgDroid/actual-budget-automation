//! Monzo bank source.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;

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

#[async_trait]
impl BankSource for Monzo {
    fn name(&self) -> &'static str {
        "monzo"
    }

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

    async fn fetch_since(&self, _since: DateTime<Utc>) -> Result<Vec<RawTransaction>, BankError> {
        // Implemented in Task 10.
        unimplemented!("fetch_since lands in Task 10")
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
}
