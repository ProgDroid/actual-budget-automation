//! Claude payee matching.

use anyhow::{Context, anyhow};
use async_trait::async_trait;
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq)]
pub struct PayeeMatch {
    pub raw: String,
    pub payee: Option<String>,
}

#[async_trait]
pub trait ClaudeMatcher {
    /// For each raw merchant, return the matched existing payee or None.
    async fn match_payees(
        &self,
        raw: &[String],
        existing_payees: &[String],
    ) -> anyhow::Result<Vec<PayeeMatch>>;
}

const SYSTEM_INSTRUCTIONS: &str = "You normalise bank merchant strings to a user's existing budget payees. \
For each raw merchant name, choose the single best-matching payee from the provided list, \
or null if none is a clear match. Be conservative: prefer null over a wrong guess. \
Call the report_matches tool with one entry per raw merchant, preserving the exact raw strings.";

/// Build the Anthropic Messages API request body. The existing-payee list is a
/// separate, cache-controlled system block so it is billed cheaply on repeat runs.
pub fn build_request(model: &str, raw: &[String], existing_payees: &[String]) -> Value {
    json!({
        "model": model,
        "max_tokens": 2048,
        "system": [
            { "type": "text", "text": SYSTEM_INSTRUCTIONS },
            {
                "type": "text",
                "text": format!("Existing payees (one per line):\n{}", existing_payees.join("\n")),
                "cache_control": { "type": "ephemeral" }
            }
        ],
        "tools": [{
            "name": "report_matches",
            "description": "Report the chosen payee for each raw merchant name.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "matches": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "raw": { "type": "string" },
                                "payee": { "type": ["string", "null"] }
                            },
                            "required": ["raw", "payee"]
                        }
                    }
                },
                "required": ["matches"]
            }
        }],
        "tool_choice": { "type": "tool", "name": "report_matches" },
        "messages": [{
            "role": "user",
            "content": format!("Raw merchant names (one per line):\n{}", raw.join("\n"))
        }]
    })
}

/// Extract `PayeeMatch`es from the Messages API response.
pub fn parse_response(resp: &Value) -> anyhow::Result<Vec<PayeeMatch>> {
    let content = resp["content"]
        .as_array()
        .context("response missing content array")?;
    let tool_use = content
        .iter()
        .find(|b| b["type"] == "tool_use" && b["name"] == "report_matches")
        .ok_or_else(|| anyhow!("no report_matches tool_use block in response"))?;
    let matches = tool_use["input"]["matches"]
        .as_array()
        .context("tool input missing matches array")?;
    let mut out = Vec::with_capacity(matches.len());
    for m in matches {
        let raw = m["raw"].as_str().context("match missing raw")?.to_string();
        let payee = match &m["payee"] {
            Value::Null => None,
            Value::String(s) => Some(s.clone()),
            other => return Err(anyhow!("unexpected payee value: {other}")),
        };
        out.push(PayeeMatch { raw, payee });
    }
    Ok(out)
}

pub struct HttpClaude {
    http: reqwest::Client,
    api_key: String,
    model: String,
    base_url: String,
}

impl HttpClaude {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key,
            model,
            base_url: "https://api.anthropic.com".to_string(),
        }
    }

    /// Override the base URL (used in tests).
    pub fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }
}

#[async_trait]
impl ClaudeMatcher for HttpClaude {
    async fn match_payees(
        &self,
        raw: &[String],
        existing_payees: &[String],
    ) -> anyhow::Result<Vec<PayeeMatch>> {
        if raw.is_empty() {
            return Ok(Vec::new());
        }
        let req = build_request(&self.model, raw, existing_payees);
        let resp = self
            .http
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&req)
            .send()
            .await
            .context("anthropic request failed")?;
        let status = resp.status();
        let value: Value = resp.json().await.context("anthropic response not JSON")?;
        if !status.is_success() {
            return Err(anyhow!("anthropic error {status}: {value}"));
        }
        parse_response(&value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_has_cached_payee_block_and_tool_choice() {
        let req = build_request(
            "claude-sonnet-4-6",
            &["TFL TRAVEL CH".to_string(), "AMZNMKTPLACE".to_string()],
            &["Transport for London".to_string(), "Amazon".to_string()],
        );
        assert_eq!(req["model"], "claude-sonnet-4-6");
        let sys = req["system"].as_array().unwrap();
        assert_eq!(sys.len(), 2);
        assert_eq!(sys[1]["cache_control"]["type"], "ephemeral");
        assert!(
            sys[1]["text"]
                .as_str()
                .unwrap()
                .contains("Transport for London")
        );
        assert_eq!(req["tool_choice"]["name"], "report_matches");
        let user = req["messages"][0]["content"].as_str().unwrap();
        assert!(user.contains("TFL TRAVEL CH"));
    }

    #[test]
    fn parse_extracts_matches_with_nulls() {
        let resp = serde_json::json!({
            "content": [
                { "type": "text", "text": "ok" },
                { "type": "tool_use", "name": "report_matches", "input": {
                    "matches": [
                        { "raw": "TFL TRAVEL CH", "payee": "Transport for London" },
                        { "raw": "AMZNMKTPLACE", "payee": null }
                    ]
                }}
            ]
        });
        let out = parse_response(&resp).unwrap();
        assert_eq!(
            out,
            vec![
                PayeeMatch {
                    raw: "TFL TRAVEL CH".into(),
                    payee: Some("Transport for London".into())
                },
                PayeeMatch {
                    raw: "AMZNMKTPLACE".into(),
                    payee: None
                },
            ]
        );
    }

    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn http_claude_posts_and_parses() {
        let server = MockServer::start().await;
        let body = serde_json::json!({
            "content": [{ "type": "tool_use", "name": "report_matches", "input": {
                "matches": [{ "raw": "TFL TRAVEL CH", "payee": "Transport for London" }]
            }}]
        });
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "test-key"))
            .and(header("anthropic-version", "2023-06-01"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let client = HttpClaude::new("test-key".into(), "claude-sonnet-4-6".into())
            .with_base_url(server.uri());
        let out = client
            .match_payees(&["TFL TRAVEL CH".into()], &["Transport for London".into()])
            .await
            .unwrap();
        assert_eq!(out[0].payee.as_deref(), Some("Transport for London"));
    }
}
