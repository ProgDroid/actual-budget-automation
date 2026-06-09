//! Claude payee matching.

use anyhow::{Context, anyhow};
use async_trait::async_trait;
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    High,
    Medium,
    Low,
}

impl Confidence {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }

    /// Higher = more confident. Used for the auto-import cutoff comparison.
    pub const fn rank(self) -> u8 {
        match self {
            Self::High => 2,
            Self::Medium => 1,
            Self::Low => 0,
        }
    }

    /// Parse the model's string; anything unrecognised is treated as Medium
    /// (conservative — it gets held for confirmation rather than auto-imported).
    pub fn parse(s: &str) -> Self {
        match s {
            "high" => Self::High,
            "low" => Self::Low,
            _ => Self::Medium,
        }
    }

    /// Strict parse for the operator-set env var. Unlike `parse` (which leniently
    /// defaults unknown Claude API output to Medium), a bad value here is a hard
    /// config error — a typo like "High"/"MEDIUM" must not silently widen
    /// auto-import.
    pub fn from_env_str(s: &str) -> anyhow::Result<Self> {
        match s {
            "high" => Ok(Self::High),
            "medium" => Ok(Self::Medium),
            "low" => Ok(Self::Low),
            other => Err(anyhow::anyhow!(
                "ABA_AUTOIMPORT_CONFIDENCE must be high|medium|low, got '{other}'"
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    pub raw: String,
    pub payee: Option<String>,
    pub payee_confidence: Confidence,
    pub category: Option<String>,
    pub category_confidence: Confidence,
}

#[async_trait]
pub trait ClaudeMatcher {
    /// For each raw merchant, return the matched existing payee and category
    /// (each `None` if no clear match), with a confidence per dimension.
    async fn classify(
        &self,
        raw: &[String],
        existing_payees: &[String],
        existing_categories: &[String],
    ) -> anyhow::Result<Vec<Classification>>;
}

const SYSTEM_INSTRUCTIONS: &str = "You normalise bank merchant strings to a user's existing budget payees and categories. \
For each raw merchant name, choose the single best-matching payee from the provided payee list (or null if none is a clear match), \
and the single best-matching category from the provided category list (or null if none is a clear match). \
Be conservative: prefer null over a wrong guess, for both payee and category. \
Rate your confidence in EACH choice independently as \"high\", \"medium\", or \"low\": \
\"high\" = an obvious, unambiguous match a human would not second-guess; \
\"medium\" = a plausible match with some doubt; \"low\" = a weak guess. \
Use \"low\" when the corresponding value is null. \
Call the report_matches tool with one entry per raw merchant, preserving the exact raw strings.";

/// Build the Anthropic Messages API request body. The existing-payee and
/// existing-category lists are separate cache-controlled system blocks so they
/// are billed cheaply on repeat runs.
pub fn build_request(
    model: &str,
    raw: &[String],
    existing_payees: &[String],
    existing_categories: &[String],
) -> Value {
    json!({
        "model": model,
        "max_tokens": 2048,
        "system": [
            { "type": "text", "text": SYSTEM_INSTRUCTIONS },
            {
                "type": "text",
                "text": format!("Existing payees (one per line):\n{}", existing_payees.join("\n")),
                "cache_control": { "type": "ephemeral" }
            },
            {
                "type": "text",
                "text": format!("Existing categories (one per line):\n{}", existing_categories.join("\n")),
                "cache_control": { "type": "ephemeral" }
            }
        ],
        "tools": [{
            "name": "report_matches",
            "description": "Report the chosen payee and category for each raw merchant name.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "matches": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "raw": { "type": "string" },
                                "payee": { "type": ["string", "null"] },
                                "payee_confidence": { "type": "string", "enum": ["high", "medium", "low"] },
                                "category": { "type": ["string", "null"] },
                                "category_confidence": { "type": "string", "enum": ["high", "medium", "low"] }
                            },
                            "required": ["raw", "payee", "payee_confidence", "category", "category_confidence"]
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

/// Extract `Classification`s from the Messages API response.
pub fn parse_response(resp: &Value) -> anyhow::Result<Vec<Classification>> {
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
    let parse_opt = |v: &Value, field: &str| -> anyhow::Result<Option<String>> {
        match &v[field] {
            Value::Null => Ok(None),
            Value::String(s) => Ok(Some(s.clone())),
            other => Err(anyhow!("unexpected {field} value: {other}")),
        }
    };
    let mut out = Vec::with_capacity(matches.len());
    for m in matches {
        let raw = m["raw"].as_str().context("match missing raw")?.to_string();
        let payee = parse_opt(m, "payee")?;
        let category = parse_opt(m, "category")?;
        let payee_confidence =
            Confidence::parse(m["payee_confidence"].as_str().unwrap_or("medium"));
        let category_confidence =
            Confidence::parse(m["category_confidence"].as_str().unwrap_or("medium"));
        out.push(Classification {
            raw,
            payee,
            payee_confidence,
            category,
            category_confidence,
        });
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
            http: crate::http::client(std::time::Duration::from_mins(2)),
            api_key,
            model,
            base_url: "https://api.anthropic.com".to_string(),
        }
    }

    #[cfg(test)]
    /// Override the base URL (used in tests).
    pub fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }
}

#[async_trait]
impl ClaudeMatcher for HttpClaude {
    async fn classify(
        &self,
        raw: &[String],
        existing_payees: &[String],
        existing_categories: &[String],
    ) -> anyhow::Result<Vec<Classification>> {
        if raw.is_empty() {
            return Ok(Vec::new());
        }
        let req = build_request(&self.model, raw, existing_payees, existing_categories);
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
            // Surface only the API's message, not the whole body (log hygiene).
            let msg = value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            return Err(anyhow!("anthropic error {status}: {msg}"));
        }
        parse_response(&value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_env_str_is_strict() {
        assert_eq!(Confidence::from_env_str("high").unwrap(), Confidence::High);
        assert_eq!(
            Confidence::from_env_str("medium").unwrap(),
            Confidence::Medium
        );
        assert_eq!(Confidence::from_env_str("low").unwrap(), Confidence::Low);
        assert!(
            Confidence::from_env_str("High").is_err(),
            "case-typo must error"
        );
        assert!(Confidence::from_env_str("bogus").is_err());
    }

    #[test]
    fn confidence_parses_and_ranks() {
        assert_eq!(Confidence::parse("high"), Confidence::High);
        assert_eq!(Confidence::parse("medium"), Confidence::Medium);
        assert_eq!(Confidence::parse("low"), Confidence::Low);
        // Unknown / missing defaults to Medium (conservative: gets held).
        assert_eq!(Confidence::parse("garbage"), Confidence::Medium);
        assert!(Confidence::High.rank() > Confidence::Medium.rank());
        assert!(Confidence::Medium.rank() > Confidence::Low.rank());
        assert_eq!(Confidence::High.rank(), 2);
        assert_eq!(Confidence::Medium.rank(), 1);
        assert_eq!(Confidence::Low.rank(), 0);
        assert_eq!(Confidence::High.as_str(), "high");
    }

    #[test]
    fn parse_reads_both_dimensions() {
        let resp = serde_json::json!({
            "content": [
                { "type": "tool_use", "name": "report_matches", "input": {
                    "matches": [
                        { "raw": "TESCO", "payee": "Tesco", "payee_confidence": "high",
                          "category": "Groceries", "category_confidence": "high" },
                        { "raw": "SQ *CAFE", "payee": "Local Cafe", "payee_confidence": "low",
                          "category": null, "category_confidence": "low" }
                    ]
                }}
            ]
        });
        let out = parse_response(&resp).unwrap();
        assert_eq!(out[0].payee.as_deref(), Some("Tesco"));
        assert_eq!(out[0].payee_confidence, Confidence::High);
        assert_eq!(out[0].category.as_deref(), Some("Groceries"));
        assert_eq!(out[0].category_confidence, Confidence::High);
        assert_eq!(out[1].category, None);
        assert_eq!(out[1].category_confidence, Confidence::Low);
    }

    #[test]
    fn request_has_cached_payee_and_category_blocks_and_schema() {
        let req = build_request(
            "claude-sonnet-4-6",
            &["TFL TRAVEL CH".to_string()],
            &["Transport for London".to_string()],
            &["Transport".to_string(), "Groceries".to_string()],
        );
        let sys = req["system"].as_array().unwrap();
        // system blocks: instructions + payees + categories
        assert_eq!(sys.len(), 3);
        assert!(
            sys[1]["text"]
                .as_str()
                .unwrap()
                .contains("Transport for London")
        );
        assert!(sys[2]["text"].as_str().unwrap().contains("Groceries"));
        assert_eq!(sys[2]["cache_control"]["type"], "ephemeral");
        let required =
            req["tools"][0]["input_schema"]["properties"]["matches"]["items"]["required"]
                .as_array()
                .unwrap();
        assert!(required.iter().any(|v| v == "category"));
        assert!(required.iter().any(|v| v == "category_confidence"));
        assert!(required.iter().any(|v| v == "payee_confidence"));
    }

    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn http_claude_posts_and_parses() {
        let server = MockServer::start().await;
        let body = serde_json::json!({
            "content": [{ "type": "tool_use", "name": "report_matches", "input": {
                "matches": [{ "raw": "TFL TRAVEL CH", "payee": "Transport for London",
                  "payee_confidence": "high", "category": "Transport", "category_confidence": "high" }]
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
            .classify(
                &["TFL TRAVEL CH".into()],
                &["Transport for London".into()],
                &["Transport".into()],
            )
            .await
            .unwrap();
        assert_eq!(out[0].payee.as_deref(), Some("Transport for London"));
        assert_eq!(out[0].category.as_deref(), Some("Transport"));
    }

    #[tokio::test]
    async fn classify_error_uses_message_not_full_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(429).set_body_json(serde_json::json!({
                "type": "error",
                "error": { "type": "rate_limit_error", "message": "Rate limited." },
                "internal_debug_blob": "DO-NOT-LOG-ME"
            })))
            .mount(&server)
            .await;
        let c = HttpClaude::new("k".into(), "m".into()).with_base_url(server.uri());
        let err = c.classify(&["X".into()], &[], &[]).await.unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("429"), "status retained: {s}");
        assert!(s.contains("Rate limited."), "message retained: {s}");
        assert!(!s.contains("DO-NOT-LOG-ME"), "full body must not leak: {s}");
    }
}
