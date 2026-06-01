//! Telegram confirm/correct channel: trait, transport, and pure parsing.

use std::collections::BTreeMap;

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
}

pub fn encode_callback(action: &str, entry_key: &str) -> String {
    let s = format!("{action}:{entry_key}");
    debug_assert!(
        s.len() <= 64,
        "callback_data exceeds Telegram's 64-byte limit"
    );
    s
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
    let row: Vec<Value> = prompt
        .buttons
        .iter()
        .map(|(label, data)| json!({ "text": label, "callback_data": data }))
        .collect();
    json!({
        "chat_id": chat_id,
        "text": prompt.chat_text,
        "parse_mode": "Markdown",
        "reply_markup": { "inline_keyboard": [row] }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn callback_roundtrips() {
        let data = encode_callback("cf", "tx_0001");
        assert_eq!(data, "cf:tx_0001");
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
}
