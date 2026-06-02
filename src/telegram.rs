//! Telegram confirm/correct channel: trait, transport, and pure parsing.

use std::collections::BTreeMap;

use anyhow::{Context, anyhow};
use async_trait::async_trait;
use serde_json::{Value, json};

/// A resolved user action against a held entry.
#[derive(Debug, Clone, PartialEq)]
pub enum ReplyAction {
    Confirm,
    CreateNew,
    Skip,
    Remap(String),
}

/// A reply correlated to a pending entry (`entry_key` = transaction external_id).
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub entry_key: String,
    pub action: ReplyAction,
}

/// A prepared prompt: text + inline-keyboard buttons (label, callback_data).
/// Built by `run` (which has the payee list); the channel only transmits it.
#[derive(Debug, Clone, PartialEq)]
pub struct Prompt {
    pub chat_text: String,
    pub buttons: Vec<(String, String)>,
}

/// Result of parsing one `getUpdates` batch.
#[derive(Debug, PartialEq)]
pub struct ParsedUpdates {
    pub replies: Vec<Reply>,
    pub new_offset: i64,
    pub ack_callback_ids: Vec<String>,
}

#[async_trait]
pub trait ConfirmChannel: Send + Sync {
    /// Drain updates newer than `since`. `msg_map` maps a prompt's message_id to
    /// the entry it was sent for, so free-text replies can be correlated.
    /// Returns (applied replies, new offset to persist).
    async fn poll_replies(
        &self,
        since: i64,
        msg_map: &BTreeMap<i64, String>,
    ) -> anyhow::Result<(Vec<Reply>, i64)>;

    /// Send a prepared prompt; returns the Telegram message_id.
    async fn send_prompt(&self, prompt: &Prompt) -> anyhow::Result<i64>;

    /// Edit a previously-sent prompt's text and drop its inline keyboard
    /// (no `reply_markup` is sent). Best-effort: callers must not abort on error.
    async fn edit_message_text(&self, message_id: i64, text: &str) -> anyhow::Result<()>;
}

#[must_use]
pub fn encode_callback(action: &str, entry_key: &str) -> Option<String> {
    let s = format!("{action}:{entry_key}");
    // Telegram caps callback_data at 64 bytes; `str::len` is the byte length.
    if s.len() > 64 {
        return None;
    }
    Some(s)
}

pub fn parse_callback(data: &str) -> Option<(String, String)> {
    let (a, k) = data.split_once(':')?;
    if a.is_empty() || k.is_empty() {
        return None;
    }
    Some((a.to_string(), k.to_string()))
}

fn action_from_code(code: &str) -> Option<ReplyAction> {
    match code {
        "cf" => Some(ReplyAction::Confirm),
        "nw" => Some(ReplyAction::CreateNew),
        "sk" => Some(ReplyAction::Skip),
        _ => None,
    }
}

/// Parse a `getUpdates` `result` array into replies + the next offset. Updates
/// from any chat other than `allowed_chat_id` are ignored, but every consumed
/// update still advances the offset (so foreign spam is never re-read forever).
pub fn parse_get_updates(
    result: &Value,
    allowed_chat_id: i64,
    msg_map: &BTreeMap<i64, String>,
) -> ParsedUpdates {
    let mut replies = Vec::new();
    let mut ack_callback_ids = Vec::new();
    let mut max_update_id: Option<i64> = None;

    if let Some(arr) = result.as_array() {
        for u in arr {
            // Telegram always sends `update_id`; this is defensive against
            // malformed input — skip anything we can't ack/order by.
            let Some(id) = u["update_id"].as_i64() else {
                continue;
            };
            max_update_id = Some(max_update_id.map_or(id, |m| m.max(id)));

            if let Some(cq) = u.get("callback_query").filter(|v| !v.is_null()) {
                if cq["message"]["chat"]["id"].as_i64() != Some(allowed_chat_id) {
                    continue;
                }
                if let Some(id) = cq["id"].as_str() {
                    // ACK even if data is absent/unparseable — clears Telegram's spinner.
                    ack_callback_ids.push(id.to_string());
                }
                if let Some(data) = cq["data"].as_str()
                    && let Some((code, key)) = parse_callback(data)
                    && let Some(action) = action_from_code(&code)
                {
                    replies.push(Reply {
                        entry_key: key,
                        action,
                    });
                }
                continue;
            }

            if let Some(m) = u.get("message").filter(|v| !v.is_null()) {
                if m["chat"]["id"].as_i64() != Some(allowed_chat_id) {
                    continue;
                }
                if let (Some(rtm), Some(text)) = (
                    m["reply_to_message"]["message_id"].as_i64(),
                    m["text"].as_str(),
                ) && let Some(key) = msg_map.get(&rtm)
                {
                    replies.push(Reply {
                        entry_key: key.clone(),
                        action: ReplyAction::Remap(text.to_string()),
                    });
                }
            }
        }
    }

    ParsedUpdates {
        new_offset: max_update_id.map_or(0, |m| m + 1),
        replies,
        ack_callback_ids,
    }
}

pub fn build_send_request(prompt: &Prompt, chat_id: i64) -> Value {
    let mut body = json!({
        "chat_id": chat_id,
        "text": prompt.chat_text,
        "parse_mode": "Markdown",
    });
    if !prompt.buttons.is_empty() {
        let row: Vec<Value> = prompt
            .buttons
            .iter()
            .map(|(label, data)| json!({ "text": label, "callback_data": data }))
            .collect();
        body["reply_markup"] = json!({ "inline_keyboard": [row] });
    }
    body
}

pub struct HttpTelegram {
    http: reqwest::Client,
    token: String,
    chat_id: i64,
    base_url: String,
}

impl HttpTelegram {
    pub fn new(token: String, chat_id: i64) -> Self {
        Self {
            http: crate::http::client(std::time::Duration::from_secs(30)),
            token,
            chat_id,
            base_url: "https://api.telegram.org".to_string(),
        }
    }

    pub fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }

    fn url(&self, method: &str) -> String {
        format!("{}/bot{}/{}", self.base_url, self.token, method)
    }

    /// Replace any occurrence of the bot token in `s` with `***`.
    fn redact(&self, s: String) -> String {
        s.replace(&self.token, "***")
    }
}

#[async_trait]
impl ConfirmChannel for HttpTelegram {
    async fn poll_replies(
        &self,
        since: i64,
        msg_map: &BTreeMap<i64, String>,
    ) -> anyhow::Result<(Vec<Reply>, i64)> {
        let resp = self
            .http
            .get(self.url("getUpdates"))
            .query(&[("offset", since.to_string()), ("timeout", "0".to_string())])
            .send()
            .await
            .map_err(|e| anyhow!("telegram getUpdates failed: {}", self.redact(e.to_string())))?;
        let status = resp.status();
        let value: Value = resp.json().await.map_err(|e| {
            anyhow!(
                "getUpdates response not JSON: {}",
                self.redact(e.to_string())
            )
        })?;
        if !status.is_success() {
            return Err(anyhow!("telegram error {status}: {value}"));
        }
        let parsed = parse_get_updates(&value["result"], self.chat_id, msg_map);

        for id in &parsed.ack_callback_ids {
            // Best-effort; failing to clear the spinner must not abort the drain.
            let _ = self
                .http
                .post(self.url("answerCallbackQuery"))
                .json(&json!({ "callback_query_id": id }))
                .send()
                .await;
        }

        // Empty batch -> keep the caller's offset (parse returns 0 for empty).
        let new_offset = if value["result"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(true)
        {
            since
        } else {
            parsed.new_offset
        };
        Ok((parsed.replies, new_offset))
    }

    async fn send_prompt(&self, prompt: &Prompt) -> anyhow::Result<i64> {
        let body = build_send_request(prompt, self.chat_id);
        let resp = self
            .http
            .post(self.url("sendMessage"))
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                anyhow!(
                    "telegram sendMessage failed: {}",
                    self.redact(e.to_string())
                )
            })?;
        let status = resp.status();
        let value: Value = resp.json().await.map_err(|e| {
            anyhow!(
                "sendMessage response not JSON: {}",
                self.redact(e.to_string())
            )
        })?;
        if !status.is_success() {
            return Err(anyhow!("telegram error {status}: {value}"));
        }
        value["result"]["message_id"]
            .as_i64()
            .context("sendMessage response missing message_id")
    }

    async fn edit_message_text(&self, message_id: i64, text: &str) -> anyhow::Result<()> {
        let body = json!({
            "chat_id": self.chat_id,
            "message_id": message_id,
            "text": text
        });
        let resp = self
            .http
            .post(self.url("editMessageText"))
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                anyhow!(
                    "telegram editMessageText failed: {}",
                    self.redact(e.to_string())
                )
            })?;
        let status = resp.status();
        let value: Value = resp.json().await.map_err(|e| {
            anyhow!(
                "editMessageText response not JSON: {}",
                self.redact(e.to_string())
            )
        })?;
        if !status.is_success() {
            return Err(anyhow!("telegram error {status}: {value}"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn callback_roundtrips() {
        let data = encode_callback("cf", "tx_0001");
        assert_eq!(data, Some("cf:tx_0001".to_string()));
        assert_eq!(
            parse_callback("cf:tx_0001"),
            Some(("cf".into(), "tx_0001".into()))
        );
        assert_eq!(parse_callback("nope"), None);
        // empty action or empty key are both rejected
        assert_eq!(parse_callback(":key"), None);
        assert_eq!(parse_callback("code:"), None);
    }

    #[test]
    fn encode_callback_rejects_over_64_bytes() {
        // 62-char key + "cf:" (3) = 65 bytes > 64 -> None.
        let long_key = "x".repeat(62);
        assert_eq!(encode_callback("cf", &long_key), None);
        // 61-char key + "cf:" = 64 bytes exactly -> Some.
        let max_key = "x".repeat(61);
        assert_eq!(
            encode_callback("cf", &max_key),
            Some(format!("cf:{max_key}"))
        );
    }

    #[test]
    fn parse_get_updates_handles_button_text_and_foreign_chat() {
        let mut msg_map = BTreeMap::new();
        msg_map.insert(555_i64, "tx_text".to_string());
        let result = serde_json::json!([
            // button tap (confirm) from the allowed chat
            { "update_id": 10, "callback_query": {
                "id": "cb1", "from": { "id": 99 },
                "message": { "chat": { "id": 99 } },
                "data": "cf:tx_btn"
            }},
            // free-text reply from the allowed chat, correlated via reply_to_message
            { "update_id": 11, "message": {
                "chat": { "id": 99 }, "text": "Sainsbury's",
                "reply_to_message": { "message_id": 555 }
            }},
            // foreign chat — must be ignored but still advance the offset
            { "update_id": 12, "message": {
                "chat": { "id": 1 }, "text": "hax",
                "reply_to_message": { "message_id": 555 }
            }}
        ]);

        let parsed = parse_get_updates(&result, 99, &msg_map);
        assert_eq!(parsed.new_offset, 13); // 12 + 1
        assert_eq!(parsed.ack_callback_ids, vec!["cb1".to_string()]);
        assert_eq!(parsed.replies.len(), 2);
        assert_eq!(parsed.replies[0].entry_key, "tx_btn");
        assert!(matches!(parsed.replies[0].action, ReplyAction::Confirm));
        assert_eq!(parsed.replies[1].entry_key, "tx_text");
        match &parsed.replies[1].action {
            ReplyAction::Remap(t) => assert_eq!(t, "Sainsbury's"),
            _ => panic!("expected remap"),
        }
    }

    #[test]
    fn build_send_request_omits_reply_markup_when_no_buttons() {
        let prompt = Prompt {
            chat_text: "no buttons".into(),
            buttons: vec![],
        };
        let body = build_send_request(&prompt, 99);
        assert_eq!(body["text"], "no buttons");
        assert!(
            body.get("reply_markup").is_none(),
            "empty buttons must not emit reply_markup, got: {body}"
        );
    }

    #[test]
    fn build_send_request_has_text_and_buttons() {
        let prompt = Prompt {
            chat_text: "hello".into(),
            buttons: vec![
                ("✅ Confirm".into(), "cf:tx1".into()),
                ("❌ Skip".into(), "sk:tx1".into()),
            ],
        };
        let body = build_send_request(&prompt, 99);
        assert_eq!(body["chat_id"], 99);
        assert_eq!(body["text"], "hello");
        let kb = &body["reply_markup"]["inline_keyboard"][0];
        assert_eq!(kb[0]["text"], "✅ Confirm");
        assert_eq!(kb[0]["callback_data"], "cf:tx1");
        assert_eq!(kb[1]["callback_data"], "sk:tx1");
    }

    #[test]
    fn parse_get_updates_empty_array_yields_zero_offset() {
        let parsed = parse_get_updates(&serde_json::json!([]), 99, &BTreeMap::new());
        assert!(parsed.replies.is_empty());
        assert_eq!(parsed.new_offset, 0);
    }

    #[test]
    fn parse_get_updates_non_array_does_not_panic() {
        for bad in [serde_json::json!(null), serde_json::json!({})] {
            let parsed = parse_get_updates(&bad, 99, &BTreeMap::new());
            assert!(parsed.replies.is_empty());
            assert_eq!(parsed.new_offset, 0);
        }
    }

    #[test]
    fn unknown_action_code_acks_but_yields_no_reply() {
        let result = serde_json::json!([
            { "update_id": 5, "callback_query": {
                "id": "cb_x", "from": { "id": 99 },
                "message": { "chat": { "id": 99 } },
                "data": "xx:tx1"
            }}
        ]);
        let parsed = parse_get_updates(&result, 99, &BTreeMap::new());
        assert!(parsed.replies.is_empty());
        assert_eq!(parsed.ack_callback_ids, vec!["cb_x".to_string()]);
        assert_eq!(parsed.new_offset, 6);
    }

    #[test]
    fn text_reply_to_unknown_message_is_dropped() {
        // msg_map does not contain the replied-to message_id -> no correlation.
        let result = serde_json::json!([
            { "update_id": 8, "message": {
                "chat": { "id": 99 }, "text": "Tesco",
                "reply_to_message": { "message_id": 777 }
            }}
        ]);
        let parsed = parse_get_updates(&result, 99, &BTreeMap::new());
        assert!(parsed.replies.is_empty());
        assert_eq!(parsed.new_offset, 9);
    }

    use wiremock::matchers::{body_json, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn http_poll_replies_parses_and_acks() {
        let server = MockServer::start().await;
        let updates = serde_json::json!({
            "ok": true,
            "result": [
                { "update_id": 7, "callback_query": {
                    "id": "cb9", "from": { "id": 42 },
                    "message": { "chat": { "id": 42 } }, "data": "sk:tx_z"
                }}
            ]
        });
        Mock::given(method("GET"))
            .and(path("/bottok/getUpdates"))
            .and(query_param("offset", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(updates))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/bottok/answerCallbackQuery"))
            .and(body_json(serde_json::json!({ "callback_query_id": "cb9" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .expect(1)
            .mount(&server)
            .await;

        let tg = HttpTelegram::new("tok".into(), 42).with_base_url(server.uri());
        let (replies, offset) = tg
            .poll_replies(0, &std::collections::BTreeMap::new())
            .await
            .unwrap();
        assert_eq!(offset, 8);
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].entry_key, "tx_z");
        assert!(matches!(replies[0].action, ReplyAction::Skip));
    }

    #[tokio::test]
    async fn http_poll_replies_empty_keeps_offset() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/bottok/getUpdates"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"ok": true, "result": []})),
            )
            .mount(&server)
            .await;
        let tg = HttpTelegram::new("tok".into(), 42).with_base_url(server.uri());
        let (replies, offset) = tg
            .poll_replies(5, &std::collections::BTreeMap::new())
            .await
            .unwrap();
        assert!(replies.is_empty());
        assert_eq!(offset, 5); // unchanged when nothing arrives
    }

    #[tokio::test]
    async fn http_send_prompt_returns_message_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bottok/sendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "ok": true, "result": { "message_id": 12345 } }),
            ))
            .mount(&server)
            .await;
        let tg = HttpTelegram::new("tok".into(), 42).with_base_url(server.uri());
        let prompt = Prompt {
            chat_text: "hi".into(),
            buttons: vec![],
        };
        let id = tg.send_prompt(&prompt).await.unwrap();
        assert_eq!(id, 12345);
    }

    #[tokio::test]
    async fn http_edit_message_text_posts_without_reply_markup() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bottok/editMessageText"))
            .and(body_json(serde_json::json!({
                "chat_id": 42,
                "message_id": 500,
                "text": "superseded"
            })))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "ok": true, "result": {} })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let tg = HttpTelegram::new("tok".into(), 42).with_base_url(server.uri());
        tg.edit_message_text(500, "superseded").await.unwrap();
    }

    // --- Token-redaction tests ---

    #[test]
    fn redact_helper_removes_token() {
        let tg = HttpTelegram::new("SUPERSECRETTOKEN".into(), 42);
        let url = "http://127.0.0.1:1/botSUPERSECRETTOKEN/getUpdates";
        let redacted = tg.redact(url.to_string());
        assert!(
            !redacted.contains("SUPERSECRETTOKEN"),
            "token must not appear after redact: {redacted}"
        );
        assert!(
            redacted.contains("***"),
            "placeholder must appear: {redacted}"
        );
    }

    #[tokio::test]
    async fn poll_replies_transport_error_does_not_leak_token() {
        // Port 1 is almost certainly connection-refused on any host.
        let tg = HttpTelegram::new("SUPERSECRETTOKEN".into(), 42)
            .with_base_url("http://127.0.0.1:1".into());
        let err = tg
            .poll_replies(0, &BTreeMap::new())
            .await
            .expect_err("connection to port 1 should fail");
        let chain = format!("{err:#}");
        assert!(
            !chain.contains("SUPERSECRETTOKEN"),
            "token leaked in poll_replies error chain: {chain}"
        );
    }

    #[tokio::test]
    async fn send_prompt_transport_error_does_not_leak_token() {
        let tg = HttpTelegram::new("SUPERSECRETTOKEN".into(), 42)
            .with_base_url("http://127.0.0.1:1".into());
        let prompt = Prompt {
            chat_text: "test".into(),
            buttons: vec![],
        };
        let err = tg
            .send_prompt(&prompt)
            .await
            .expect_err("connection to port 1 should fail");
        let chain = format!("{err:#}");
        assert!(
            !chain.contains("SUPERSECRETTOKEN"),
            "token leaked in send_prompt error chain: {chain}"
        );
    }
}
