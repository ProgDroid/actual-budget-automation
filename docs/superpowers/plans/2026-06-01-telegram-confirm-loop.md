# Telegram Confirm/Correct Loop — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an asynchronous two-way Telegram channel so uncertain Claude payee matches are confirmed/corrected from a phone before import, via a new frequent one-shot `aba poll` subcommand (drains Telegram, write-free) feeding the nightly `aba run` (sole Actual writer).

**Architecture:** Confidence-gate Claude matches (`high` auto-imports as today; `medium`/`low` held as `pending_review` with a suggestion). A write-free `aba poll` (cron ~15 min) drains Telegram `getUpdates`, applies replies to the state file, and advances the offset; the nightly `aba run` imports resolved items, sends prompts for new held items, and resolves free-text remaps against the payee list it already loads. Two one-shot processes serialise on a native OS file lock. Telegram lives behind a `ConfirmChannel` trait (HTTP impl + test double), mirroring `Notifier`/`ClaudeMatcher`.

**Tech Stack:** Rust 2024 (rustc 1.95 — native `std::fs::File::lock`/`try_lock`, no locking crate), tokio, reqwest (rustls), serde/serde_json, toml, chrono, clap, async-trait, strsim, tracing. Tests: wiremock, tempfile.

**Reference docs the engineer should skim:**
- Spec: `docs/superpowers/specs/2026-06-01-telegram-confirm-loop-design.md`
- Existing patterns to mirror: `src/notify.rs` (trait + HTTP impl + wiremock test), `src/claude.rs` (pure `build_request`/`parse_response` + `HttpClaude` with `with_base_url` test override), `src/state.rs` (TOML round-trip tests).
- Telegram Bot API: `GET /bot<token>/getUpdates?offset=<n>&timeout=0` returns `{ok, result:[{update_id, message?, callback_query?}]}`. Offset to ack = highest `update_id` + 1; updates are dropped after 24h. `callback_query` = button tap (`data` ≤ 64 bytes, `id` to `answerCallbackQuery`, `message.chat.id`, `from.id`). `message.reply_to_message.message_id` correlates a free-text reply to the prompt it answers. `POST /bot<token>/sendMessage {chat_id, text, parse_mode, reply_markup:{inline_keyboard:[[{text, callback_data}]]}}` → `{ok, result:{message_id}}`. `POST /bot<token>/answerCallbackQuery {callback_query_id}` clears the client spinner.

---

## File structure

```
Cargo.toml                 # NO new deps (native file lock); confirm only
src/
  main.rs                  # + Poll dispatch, ConfirmChannel construction, file locks
  config.rs                # + telegram_*, autoimport_confidence, lock_path, telegram_base_url; + Command::Poll
  claude.rs                # + Confidence enum, PayeeMatch.confidence, schema + parse
  state.rs                 # + PendingReview.{suggestion,confidence,telegram_message_id}, TelegramState, mutators
  matching.rs              # + best_payee_match()
  telegram.rs   (NEW)      # ConfirmChannel trait, Reply/ReplyAction, Prompt, parse_get_updates, build_send_request, HttpTelegram
  lock.rs       (NEW)      # lock_blocking() / lock_try() over std::fs::File
  run.rs                   # confidence partition, remap normalisation, build_prompt, send prompts, reminder count
  poll.rs       (NEW)      # poll(): drain replies -> state (write-free)
docs/
  RUNBOOK.md               # + two cron lines, new env vars, bot setup
```

Each task ends green (tests pass) and is committed. Run the whole suite with `cargo test` and lints with `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check`.

---

## Task 1: Confidence signal on Claude matches

**Files:**
- Modify: `src/claude.rs`

- [ ] **Step 1: Write failing tests for the `Confidence` enum and parsing**

Add to the `tests` module in `src/claude.rs`:

```rust
    #[test]
    fn confidence_parses_and_ranks() {
        assert_eq!(Confidence::parse("high"), Confidence::High);
        assert_eq!(Confidence::parse("medium"), Confidence::Medium);
        assert_eq!(Confidence::parse("low"), Confidence::Low);
        // Unknown / missing defaults to Medium (conservative: gets held).
        assert_eq!(Confidence::parse("garbage"), Confidence::Medium);
        assert!(Confidence::High.rank() > Confidence::Medium.rank());
        assert!(Confidence::Medium.rank() > Confidence::Low.rank());
        assert_eq!(Confidence::High.as_str(), "high");
    }

    #[test]
    fn parse_reads_confidence_field() {
        let resp = serde_json::json!({
            "content": [
                { "type": "tool_use", "name": "report_matches", "input": {
                    "matches": [
                        { "raw": "TFL TRAVEL CH", "payee": "Transport for London", "confidence": "high" },
                        { "raw": "SQ *CAFE", "payee": "Local Cafe", "confidence": "low" },
                        { "raw": "WUT", "payee": null, "confidence": "high" }
                    ]
                }}
            ]
        });
        let out = parse_response(&resp).unwrap();
        assert_eq!(out[0].confidence, Confidence::High);
        assert_eq!(out[1].confidence, Confidence::Low);
        assert_eq!(out[2].payee, None);
    }
```

- [ ] **Step 2: Run tests, verify they fail**

Run: `cargo test --lib claude`
Expected: FAIL — `Confidence` not defined, `PayeeMatch` has no `confidence` field.

- [ ] **Step 3: Add the `Confidence` enum and field**

In `src/claude.rs`, replace the `PayeeMatch` struct (lines 7–11) with:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    High,
    Medium,
    Low,
}

impl Confidence {
    pub fn as_str(&self) -> &'static str {
        match self {
            Confidence::High => "high",
            Confidence::Medium => "medium",
            Confidence::Low => "low",
        }
    }

    /// Higher = more confident. Used for the auto-import cutoff comparison.
    pub fn rank(&self) -> u8 {
        match self {
            Confidence::High => 2,
            Confidence::Medium => 1,
            Confidence::Low => 0,
        }
    }

    /// Parse the model's string; anything unrecognised is treated as Medium
    /// (conservative — it gets held for confirmation rather than auto-imported).
    pub fn parse(s: &str) -> Confidence {
        match s {
            "high" => Confidence::High,
            "low" => Confidence::Low,
            _ => Confidence::Medium,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PayeeMatch {
    pub raw: String,
    pub payee: Option<String>,
    pub confidence: Confidence,
}
```

- [ ] **Step 4: Update the system prompt, tool schema, and parser**

In `src/claude.rs`, replace `SYSTEM_INSTRUCTIONS` (lines 23–26) with:

```rust
const SYSTEM_INSTRUCTIONS: &str = "You normalise bank merchant strings to a user's existing budget payees. \
For each raw merchant name, choose the single best-matching payee from the provided list, \
or null if none is a clear match. Be conservative: prefer null over a wrong guess. \
Also rate your confidence in each choice as \"high\", \"medium\", or \"low\": \
\"high\" = an obvious, unambiguous match a human would not second-guess; \
\"medium\" = a plausible match with some doubt; \"low\" = a weak guess. \
Use \"low\" when payee is null. \
Call the report_matches tool with one entry per raw merchant, preserving the exact raw strings.";
```

In the tool `input_schema` `properties` of each match item (lines 53–58), add the `confidence` property and mark it required:

```rust
                            "properties": {
                                "raw": { "type": "string" },
                                "payee": { "type": ["string", "null"] },
                                "confidence": { "type": "string", "enum": ["high", "medium", "low"] }
                            },
                            "required": ["raw", "payee", "confidence"]
```

In `parse_response` (the per-match loop, lines 84–92), read the confidence:

```rust
        let raw = m["raw"].as_str().context("match missing raw")?.to_string();
        let payee = match &m["payee"] {
            Value::Null => None,
            Value::String(s) => Some(s.clone()),
            other => return Err(anyhow!("unexpected payee value: {other}")),
        };
        let confidence = Confidence::parse(m["confidence"].as_str().unwrap_or("medium"));
        out.push(PayeeMatch {
            raw,
            payee,
            confidence,
        });
```

- [ ] **Step 5: Fix the existing schema assertion test and the `parse_extracts_matches_with_nulls` test**

The existing `parse_extracts_matches_with_nulls` test builds `PayeeMatch` without `confidence` and its JSON lacks the field. Update its expected vector to include confidence and add the field to the JSON:

```rust
    #[test]
    fn parse_extracts_matches_with_nulls() {
        let resp = serde_json::json!({
            "content": [
                { "type": "text", "text": "ok" },
                { "type": "tool_use", "name": "report_matches", "input": {
                    "matches": [
                        { "raw": "TFL TRAVEL CH", "payee": "Transport for London", "confidence": "high" },
                        { "raw": "AMZNMKTPLACE", "payee": null, "confidence": "low" }
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
                    payee: Some("Transport for London".into()),
                    confidence: Confidence::High,
                },
                PayeeMatch {
                    raw: "AMZNMKTPLACE".into(),
                    payee: None,
                    confidence: Confidence::Low,
                },
            ]
        );
    }
```

In `request_has_cached_payee_block_and_tool_choice`, add an assertion that the schema now requires confidence (append before the closing brace):

```rust
        let required = req["tools"][0]["input_schema"]["properties"]["matches"]["items"]["required"]
            .as_array()
            .unwrap();
        assert!(required.iter().any(|v| v == "confidence"));
```

In the `http_claude_posts_and_parses` wiremock test, add `"confidence": "high"` to the match object in the mocked response body.

- [ ] **Step 6: Run tests, verify they pass**

Run: `cargo test --lib claude`
Expected: PASS (all claude tests).

- [ ] **Step 7: Commit**

```bash
git add src/claude.rs
git commit -m "feat(claude): per-match confidence enum in tool schema + parse"
```

---

## Task 2: State schema — held suggestions, Telegram offset, reply mutators

**Files:**
- Modify: `src/state.rs`

- [ ] **Step 1: Write failing tests**

Add to the `tests` module in `src/state.rs`:

```rust
    #[test]
    fn roundtrips_new_pending_fields_and_telegram_offset() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.toml");
        let mut s = State::default();
        s.telegram.last_update_id = 42;
        s.pending_reviews.push(PendingReview {
            external_id: "tx1".into(),
            raw_merchant: "TFL TRAVEL CH".into(),
            amount: -275,
            date: "2026-05-30".into(),
            resolve_to: String::new(),
            create_new: false,
            suggestion: Some("Transport for London".into()),
            confidence: Some("medium".into()),
            telegram_message_id: Some(1001),
        });
        s.save(&path).unwrap();
        let loaded = State::load(&path).unwrap();
        assert_eq!(loaded, s);
    }

    #[test]
    fn add_pending_proposed_carries_suggestion() {
        let mut s = State::default();
        let added = s.add_pending_full(
            "tx2",
            "SQ *CAFE",
            -450,
            "2026-05-31",
            Some("Local Cafe".into()),
            Some("low".into()),
        );
        assert!(added);
        let p = &s.pending_reviews[0];
        assert_eq!(p.suggestion.as_deref(), Some("Local Cafe"));
        assert_eq!(p.confidence.as_deref(), Some("low"));
        assert!(p.telegram_message_id.is_none());
    }

    #[test]
    fn reply_mutators_apply() {
        let mut s = State::default();
        s.add_pending_full("a", "AAA", -1, "2026-06-01", Some("Aldi".into()), Some("medium".into()));
        s.add_pending_full("b", "BBB", -2, "2026-06-01", None, None);
        s.add_pending_full("c", "CCC", -3, "2026-06-01", Some("Costa".into()), Some("low".into()));
        s.add_pending_full("d", "DDD", -4, "2026-06-01", None, None);

        assert!(s.confirm_pending("a"));            // resolve_to <- suggestion
        assert!(s.remap_pending("b", "Boots"));     // resolve_to <- free text
        assert!(s.create_pending("c"));             // create_new = true
        assert!(s.skip_pending("d"));               // removed
        assert!(!s.confirm_pending("missing"));

        let by = |id: &str| s.pending_reviews.iter().find(|p| p.external_id == id);
        assert_eq!(by("a").unwrap().resolve_to, "Aldi");
        assert_eq!(by("b").unwrap().resolve_to, "Boots");
        assert!(by("c").unwrap().create_new);
        assert!(by("d").is_none());
    }
```

- [ ] **Step 2: Run tests, verify they fail**

Run: `cargo test --lib state`
Expected: FAIL — fields and methods do not exist.

- [ ] **Step 3: Add the new fields and `TelegramState`**

In `src/state.rs`, add to the `State` struct (after `pending_reviews`):

```rust
    #[serde(default)]
    pub telegram: TelegramState,
```

Add the struct near `MonzoState`:

```rust
#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TelegramState {
    /// Highest acknowledged getUpdates offset; advanced only by `aba poll`.
    #[serde(default)]
    pub last_update_id: i64,
}
```

Add the three fields to `PendingReview` (after `create_new`):

```rust
    /// Claude's proposed payee for a medium/low-confidence held match.
    /// `None` = a genuine no-match (today's behavior).
    #[serde(default)]
    pub suggestion: Option<String>,
    /// "medium" | "low" when this was a confidence-held proposal.
    #[serde(default)]
    pub confidence: Option<String>,
    /// `Some` once a Telegram prompt has been sent (idempotency guard).
    #[serde(default)]
    pub telegram_message_id: Option<i64>,
```

- [ ] **Step 4: Update `add_pending` and add the new methods**

Replace the existing `add_pending` body (lines 113–136) so it delegates to a fuller method, and add the reply mutators. Note the existing struct literal in `add_pending` must gain the three new fields:

```rust
    /// Add a pending review unless an entry with the same `external_id` already
    /// exists. Returns true if added.
    pub fn add_pending(
        &mut self,
        external_id: &str,
        raw_merchant: &str,
        amount: i64,
        date: &str,
    ) -> bool {
        self.add_pending_full(external_id, raw_merchant, amount, date, None, None)
    }

    /// Add a pending review with an optional Claude suggestion + confidence.
    pub fn add_pending_full(
        &mut self,
        external_id: &str,
        raw_merchant: &str,
        amount: i64,
        date: &str,
        suggestion: Option<String>,
        confidence: Option<String>,
    ) -> bool {
        if self
            .pending_reviews
            .iter()
            .any(|p| p.external_id == external_id)
        {
            return false;
        }
        self.pending_reviews.push(PendingReview {
            external_id: external_id.to_string(),
            raw_merchant: raw_merchant.to_string(),
            amount,
            date: date.to_string(),
            resolve_to: String::new(),
            create_new: false,
            suggestion,
            confidence,
            telegram_message_id: None,
        });
        true
    }

    /// Confirm a proposed match: copy its `suggestion` into `resolve_to`.
    pub fn confirm_pending(&mut self, external_id: &str) -> bool {
        if let Some(p) = self
            .pending_reviews
            .iter_mut()
            .find(|p| p.external_id == external_id)
            && let Some(s) = p.suggestion.clone()
        {
            p.resolve_to = s;
            return true;
        }
        false
    }

    /// Remap to a free-text payee name (resolved against the payee list later).
    pub fn remap_pending(&mut self, external_id: &str, text: &str) -> bool {
        if let Some(p) = self
            .pending_reviews
            .iter_mut()
            .find(|p| p.external_id == external_id)
        {
            p.resolve_to = text.to_string();
            p.create_new = false;
            return true;
        }
        false
    }

    /// Mark a pending entry to be created as a new payee.
    pub fn create_pending(&mut self, external_id: &str) -> bool {
        if let Some(p) = self
            .pending_reviews
            .iter_mut()
            .find(|p| p.external_id == external_id)
        {
            p.create_new = true;
            return true;
        }
        false
    }

    /// Drop a pending entry (the user skipped it). Does not touch the cache.
    pub fn skip_pending(&mut self, external_id: &str) -> bool {
        let before = self.pending_reviews.len();
        self.pending_reviews
            .retain(|p| p.external_id != external_id);
        self.pending_reviews.len() != before
    }
```

Also update the existing `take_resolved` create-new naming (lines 75–81) to prefer `suggestion` when `resolve_to` is empty, so a "create" tap on a proposal/failed-remap uses the suggested name:

```rust
            let (payee_name, create_new) = if pr.create_new {
                let name = if !pr.resolve_to.is_empty() {
                    pr.resolve_to.clone()
                } else if let Some(s) = &pr.suggestion {
                    s.clone()
                } else {
                    pr.raw_merchant.clone()
                };
                (name, true)
            } else if !pr.resolve_to.is_empty() {
```

- [ ] **Step 5: Fix existing `PendingReview` literals in tests**

The existing tests `save_then_load_roundtrips` and `take_resolved_maps_existing_and_creates_and_keeps_unresolved` build `PendingReview { ... }` literals without the three new fields. Add `suggestion: None, confidence: None, telegram_message_id: None,` to each of those literals.

- [ ] **Step 6: Run tests, verify they pass**

Run: `cargo test --lib state`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/state.rs
git commit -m "feat(state): held suggestions, telegram offset, reply mutators"
```

---

## Task 3: `best_payee_match` for free-text remap resolution

**Files:**
- Modify: `src/matching.rs`

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/matching.rs`:

```rust
    #[test]
    fn best_payee_match_picks_closest_above_threshold() {
        let payees = vec![
            "Transport for London".to_string(),
            "Tesco".to_string(),
            "Amazon".to_string(),
        ];
        // exact (case/space-insensitive via normalise) wins
        assert_eq!(
            best_payee_match("tesco", &payees, 0.85).as_deref(),
            Some("Tesco")
        );
        // close typo resolves to nearest
        assert_eq!(
            best_payee_match("Tescos", &payees, 0.85).as_deref(),
            Some("Tesco")
        );
        // nothing close enough -> None (caller re-holds)
        assert_eq!(best_payee_match("Sainsbury's", &payees, 0.85), None);
    }
```

- [ ] **Step 2: Run test, verify it fails**

Run: `cargo test --lib matching::tests::best_payee_match`
Expected: FAIL — function not found.

- [ ] **Step 3: Implement `best_payee_match`**

Add to `src/matching.rs` after `payee_similarity`:

```rust
/// Return the existing payee most similar to `text`, if any clears `threshold`.
/// Used to resolve a free-text Telegram remap reply to a real payee.
pub fn best_payee_match(text: &str, payees: &[String], threshold: f64) -> Option<String> {
    payees
        .iter()
        .map(|p| (p, payee_similarity(text, p)))
        .filter(|(_, sim)| *sim >= threshold)
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(p, _)| p.clone())
}
```

- [ ] **Step 4: Run test, verify it passes**

Run: `cargo test --lib matching`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/matching.rs
git commit -m "feat(matching): best_payee_match for free-text remap resolution"
```

---

## Task 4: Telegram pure logic — types, callback codec, update parsing, prompt body

**Files:**
- Create: `src/telegram.rs`
- Modify: `src/main.rs` (add `mod telegram;`)

- [ ] **Step 1: Register the module**

In `src/main.rs`, add `mod telegram;` with the other `mod` lines (keep alphabetical-ish ordering; after `mod state;` is fine).

- [ ] **Step 2: Write failing tests for the pure functions**

Create `src/telegram.rs` with only the test module first (implementation comes next):

```rust
//! Telegram confirm/correct channel: trait, transport, and pure parsing.

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn callback_roundtrips() {
        let data = encode_callback("cf", "tx_0001");
        assert_eq!(data, "cf:tx_0001");
        assert_eq!(parse_callback("cf:tx_0001"), Some(("cf".into(), "tx_0001".into())));
        assert_eq!(parse_callback("nope"), None);
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
}
```

- [ ] **Step 3: Run tests, verify they fail**

Run: `cargo test --lib telegram`
Expected: FAIL — items not defined.

- [ ] **Step 4: Implement the types and pure functions**

Insert above the `tests` module in `src/telegram.rs`:

```rust
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
    format!("{action}:{entry_key}")
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
            if let Some(id) = u["update_id"].as_i64() {
                max_update_id = Some(max_update_id.map_or(id, |m| m.max(id)));
            }

            if let Some(cq) = u.get("callback_query").filter(|v| !v.is_null()) {
                if cq["message"]["chat"]["id"].as_i64() != Some(allowed_chat_id) {
                    continue;
                }
                if let Some(id) = cq["id"].as_str() {
                    ack_callback_ids.push(id.to_string());
                }
                if let Some(data) = cq["data"].as_str()
                    && let Some((code, key)) = parse_callback(data)
                    && let Some(action) = action_from_code(&code)
                {
                    replies.push(Reply { entry_key: key, action });
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
```

Note: `parse_get_updates` returns `new_offset = 0` for an empty batch; callers that get no updates keep their stored offset instead (see Task 5).

- [ ] **Step 5: Run tests, verify they pass**

Run: `cargo test --lib telegram`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/telegram.rs src/main.rs
git commit -m "feat(telegram): reply types, callback codec, update parsing, prompt body"
```

---

## Task 5: `HttpTelegram` transport (wiremock)

**Files:**
- Modify: `src/telegram.rs`

- [ ] **Step 1: Write failing wiremock tests**

Add to the `tests` module in `src/telegram.rs`:

```rust
    use wiremock::matchers::{method, path, query_param};
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
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .expect(1)
            .mount(&server)
            .await;

        let tg = HttpTelegram::new("tok".into(), 42).with_base_url(server.uri());
        let (replies, offset) = tg.poll_replies(0, &std::collections::BTreeMap::new()).await.unwrap();
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
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true, "result": []})))
            .mount(&server)
            .await;
        let tg = HttpTelegram::new("tok".into(), 42).with_base_url(server.uri());
        let (replies, offset) = tg.poll_replies(5, &std::collections::BTreeMap::new()).await.unwrap();
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
        let prompt = Prompt { chat_text: "hi".into(), buttons: vec![] };
        let id = tg.send_prompt(&prompt).await.unwrap();
        assert_eq!(id, 12345);
    }
```

- [ ] **Step 2: Run tests, verify they fail**

Run: `cargo test --lib telegram`
Expected: FAIL — `HttpTelegram` not defined.

- [ ] **Step 3: Implement `HttpTelegram`**

Add to `src/telegram.rs` (above the tests module). Note the `anyhow` import addition at the top of the file: change the existing `use serde_json::{Value, json};` area to also bring in `anyhow::{Context, anyhow}`.

```rust
use anyhow::{Context, anyhow};

pub struct HttpTelegram {
    http: reqwest::Client,
    token: String,
    chat_id: i64,
    base_url: String,
}

impl HttpTelegram {
    pub fn new(token: String, chat_id: i64) -> Self {
        Self {
            http: reqwest::Client::new(),
            token,
            chat_id,
            base_url: "https://api.telegram.org".to_string(),
        }
    }

    #[cfg(test)]
    pub fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }

    fn url(&self, method: &str) -> String {
        format!("{}/bot{}/{}", self.base_url, self.token, method)
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
            .context("telegram getUpdates failed")?;
        let status = resp.status();
        let value: Value = resp.json().await.context("getUpdates response not JSON")?;
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
        let new_offset = if value["result"].as_array().map(|a| a.is_empty()).unwrap_or(true) {
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
            .context("telegram sendMessage failed")?;
        let status = resp.status();
        let value: Value = resp.json().await.context("sendMessage response not JSON")?;
        if !status.is_success() {
            return Err(anyhow!("telegram error {status}: {value}"));
        }
        value["result"]["message_id"]
            .as_i64()
            .context("sendMessage response missing message_id")
    }
}
```

- [ ] **Step 4: Run tests, verify they pass**

Run: `cargo test --lib telegram`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/telegram.rs
git commit -m "feat(telegram): HttpTelegram transport (getUpdates/sendMessage)"
```

---

## Task 6: Native file lock helpers

**Files:**
- Create: `src/lock.rs`
- Modify: `src/main.rs` (add `mod lock;`)

- [ ] **Step 1: Register the module**

In `src/main.rs`, add `mod lock;`.

- [ ] **Step 2: Write the failing test**

Create `src/lock.rs`:

```rust
//! Cross-platform advisory file locking via std (Rust 1.89+ stabilised
//! `File::lock`/`try_lock`). Used to serialise `aba run` and `aba poll`, which
//! both read-modify-write the shared TOML state file.

use std::fs::{File, TryLockError};
use std::path::Path;

/// Acquire an exclusive advisory lock, blocking until available. The returned
/// `File` holds the lock until it is dropped.
pub fn lock_blocking(path: &Path) -> anyhow::Result<File> {
    let f = File::create(path)?;
    f.lock()?;
    Ok(f)
}

/// Try to acquire the lock without blocking. `Ok(Some(file))` = acquired,
/// `Ok(None)` = currently held by another handle/process.
pub fn lock_try(path: &Path) -> anyhow::Result<Option<File>> {
    let f = File::create(path)?;
    match f.try_lock() {
        Ok(()) => Ok(Some(f)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn try_lock_blocks_while_held() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("aba.lock");

        let held = lock_blocking(&path).unwrap();
        // A second handle to the same file cannot acquire while `held` lives.
        assert!(lock_try(&path).unwrap().is_none());

        drop(held);
        // Once released, it can be acquired again.
        assert!(lock_try(&path).unwrap().is_some());
    }
}
```

- [ ] **Step 3: Run test, verify it fails then passes**

Run: `cargo test --lib lock`
Expected: this file is implementation + test together; it should compile and PASS. If `TryLockError` import errors, confirm toolchain ≥ 1.89 with `rustc --version` (this repo uses 1.95).

- [ ] **Step 4: Commit**

```bash
git add src/lock.rs src/main.rs
git commit -m "feat(lock): native advisory file-lock helpers for run/poll"
```

---

## Task 7: Config — new env vars and the `poll` subcommand

**Files:**
- Modify: `src/config.rs`

- [ ] **Step 1: Write failing tests**

Add to the `tests` module in `src/config.rs`:

```rust
    #[test]
    fn telegram_and_autoimport_defaults_and_overrides() {
        let base = |k: &str| -> Option<String> {
            match k {
                "ANTHROPIC_API_KEY" => Some("ak".into()),
                "MONZO_CLIENT_ID" => Some("cid".into()),
                "MONZO_CLIENT_SECRET" => Some("csec".into()),
                "MONZO_REDIRECT_URI" => Some("http://localhost:8080/callback".into()),
                "APPRISE_URL" => Some("http://apprise/notify".into()),
                "ACTUAL_SERVER_URL" => Some("http://actual:5006".into()),
                "ACTUAL_PASSWORD" => Some("pw".into()),
                "ACTUAL_SYNC_ID" => Some("sync".into()),
                "ABA_STATE_PATH" => Some("/data/aba.state.toml".into()),
                _ => None,
            }
        };
        let c = Config::from_getter(base).unwrap();
        assert_eq!(c.telegram_bot_token, None);
        assert_eq!(c.telegram_chat_id, None);
        assert_eq!(c.autoimport_confidence, "high");
        assert_eq!(c.lock_path.to_string_lossy(), "/data/aba.state.lock");
        assert_eq!(c.telegram_base_url, "https://api.telegram.org");

        let with_tg = |k: &str| -> Option<String> {
            match k {
                "TELEGRAM_BOT_TOKEN" => Some("tok".into()),
                "TELEGRAM_CHAT_ID" => Some("42".into()),
                "ABA_AUTOIMPORT_CONFIDENCE" => Some("medium".into()),
                _ => base(k),
            }
        };
        let c2 = Config::from_getter(with_tg).unwrap();
        assert_eq!(c2.telegram_bot_token.as_deref(), Some("tok"));
        assert_eq!(c2.telegram_chat_id.as_deref(), Some("42"));
        assert_eq!(c2.autoimport_confidence, "medium");
    }
```

- [ ] **Step 2: Run test, verify it fails**

Run: `cargo test --lib config`
Expected: FAIL — fields do not exist.

- [ ] **Step 3: Add `Command::Poll` and the config fields**

In `src/config.rs`, add a variant to `Command`:

```rust
    /// Drain Telegram replies into the state file (run frequently; write-free re: Actual).
    Poll,
```

Add fields to `Config` (after `monzo_base_url`):

```rust
    pub telegram_bot_token: Option<String>,
    pub telegram_chat_id: Option<String>,
    pub autoimport_confidence: String,
    pub lock_path: PathBuf,
    pub telegram_base_url: String,
```

In `from_getter`, after `monzo_base_url`, compute and add:

```rust
            telegram_bot_token: get("TELEGRAM_BOT_TOKEN"),
            telegram_chat_id: get("TELEGRAM_CHAT_ID"),
            autoimport_confidence: get("ABA_AUTOIMPORT_CONFIDENCE")
                .unwrap_or_else(|| "high".to_string()),
            lock_path: get("ABA_LOCK_PATH").map(PathBuf::from).unwrap_or_else(|| {
                PathBuf::from(req_state.clone()).with_extension("lock")
            }),
            telegram_base_url: get("TELEGRAM_BASE_URL")
                .unwrap_or_else(|| "https://api.telegram.org".to_string()),
```

To compute `lock_path` from the state path string, capture it once before the struct literal. Replace the `state_path: PathBuf::from(req("ABA_STATE_PATH")?),` line with a binding above the `Ok(Config {`:

```rust
        let req_state = req("ABA_STATE_PATH")?;
        Ok(Config {
            ...
            state_path: PathBuf::from(req_state.clone()),
            ...
        })
```

(The `req_state.clone()` is reused by `lock_path`.)

- [ ] **Step 4: Run tests, verify they pass**

Run: `cargo test --lib config`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/config.rs
git commit -m "feat(config): telegram vars, autoimport cutoff, lock path, poll command"
```

---

## Task 8: `run` — confidence partition, remap resolution, prompts, reminder

**Files:**
- Modify: `src/run.rs`

This task changes `MatchOutcome`/`apply_matches`, adds `build_prompt`, threads a `ConfirmChannel` and the autoimport cutoff through `run`, normalises remap replies, sends prompts idempotently, and appends the reminder count. It also adds a `ConfirmChannel` test double reused by Task 9 and Task 10.

- [ ] **Step 1: Write failing unit tests for the partition + prompt builder**

Replace the existing `applies_claude_matches_and_flags_unmatched` test in `src/run.rs` and add new ones. In the `tests` module, update the `tx` helper is unchanged; add:

```rust
    use crate::claude::Confidence;
    use crate::state::PendingReview;
    use crate::telegram::Prompt;

    #[test]
    fn apply_matches_partitions_by_confidence() {
        let txs = vec![
            tx("1", "TESCO", -500),       // high -> auto import
            tx("2", "SQ *CAFE", -450),    // medium -> proposed/held
            tx("3", "WEIRD XYZ", -100),   // none -> unmatched/held
        ];
        let matches = vec![
            PayeeMatch { raw: "TESCO".into(), payee: Some("Tesco".into()), confidence: Confidence::High },
            PayeeMatch { raw: "SQ *CAFE".into(), payee: Some("Local Cafe".into()), confidence: Confidence::Medium },
            PayeeMatch { raw: "WEIRD XYZ".into(), payee: None, confidence: Confidence::Low },
        ];
        let outcome = apply_matches(&txs, &matches, Confidence::High);
        assert_eq!(outcome.matched, vec![("1".to_string(), "Tesco".to_string())]);
        assert_eq!(outcome.proposed.len(), 1);
        assert_eq!(outcome.proposed[0].0.external_id, "2");
        assert_eq!(outcome.proposed[0].1, "Local Cafe");
        assert_eq!(outcome.proposed[0].2, Confidence::Medium);
        assert_eq!(outcome.unmatched.len(), 1);
        assert_eq!(outcome.unmatched[0].external_id, "3");
    }

    #[test]
    fn build_prompt_picks_flavor_by_payee_membership() {
        let payees = vec!["Tesco".to_string(), "Local Cafe".to_string()];

        // proposed, suggestion exists -> confirm/create/skip
        let proposed = PendingReview {
            external_id: "tx1".into(), raw_merchant: "SQ *CAFE".into(), amount: -450,
            date: "2026-06-01".into(), resolve_to: String::new(), create_new: false,
            suggestion: Some("Local Cafe".into()), confidence: Some("medium".into()),
            telegram_message_id: None,
        };
        let p = build_prompt(&proposed, &payees);
        assert!(p.chat_text.contains("Local Cafe"));
        assert_eq!(p.buttons.len(), 3);
        assert_eq!(p.buttons[0].1, "cf:tx1");

        // no-match -> create/skip only
        let unmatched = PendingReview {
            external_id: "tx2".into(), raw_merchant: "MYSTERY".into(), amount: -10,
            date: "2026-06-01".into(), resolve_to: String::new(), create_new: false,
            suggestion: None, confidence: None, telegram_message_id: None,
        };
        let p2 = build_prompt(&unmatched, &payees);
        assert_eq!(p2.buttons.len(), 2);
        assert_eq!(p2.buttons[0].1, "nw:tx2");

        // failed remap: suggestion set but NOT in payees -> "create it?" flavor
        let failed = PendingReview {
            external_id: "tx3".into(), raw_merchant: "X".into(), amount: -10,
            date: "2026-06-01".into(), resolve_to: String::new(), create_new: false,
            suggestion: Some("Sainsbury's".into()), confidence: None, telegram_message_id: None,
        };
        let p3 = build_prompt(&failed, &payees);
        assert!(p3.chat_text.contains("Sainsbury's"));
        assert_eq!(p3.buttons.len(), 2);
        assert_eq!(p3.buttons[0].1, "nw:tx3"); // create
        assert_eq!(p3.buttons[1].1, "sk:tx3"); // skip
    }
```

- [ ] **Step 2: Run tests, verify they fail**

Run: `cargo test --lib run`
Expected: FAIL — new `MatchOutcome` shape, `apply_matches` arity, `build_prompt` missing.

- [ ] **Step 3: Rewrite `MatchOutcome` + `apply_matches`**

In `src/run.rs`, replace the `MatchOutcome` struct and `apply_matches` (lines 44–66) with:

```rust
/// Result of applying Claude matches, split by the auto-import cutoff.
#[derive(Debug, PartialEq)]
pub struct MatchOutcome {
    /// (external_id, payee_name) confident enough to import + cache now.
    pub matched: Vec<(String, String)>,
    /// (tx, suggested_payee, confidence) held for Telegram confirmation.
    pub proposed: Vec<(RawTransaction, String, Confidence)>,
    /// No usable match — held with no suggestion.
    pub unmatched: Vec<RawTransaction>,
}

pub fn apply_matches(
    txs: &[RawTransaction],
    matches: &[PayeeMatch],
    autoimport: Confidence,
) -> MatchOutcome {
    let lookup: BTreeMap<&str, &PayeeMatch> =
        matches.iter().map(|m| (m.raw.as_str(), m)).collect();
    let mut matched = Vec::new();
    let mut proposed = Vec::new();
    let mut unmatched = Vec::new();
    for tx in txs {
        match lookup.get(tx.raw_merchant.as_str()) {
            Some(m) => match &m.payee {
                Some(payee) if m.confidence.rank() >= autoimport.rank() => {
                    matched.push((tx.external_id.clone(), payee.clone()))
                }
                Some(payee) => proposed.push((tx.clone(), payee.clone(), m.confidence)),
                None => unmatched.push(tx.clone()),
            },
            None => unmatched.push(tx.clone()),
        }
    }
    MatchOutcome {
        matched,
        proposed,
        unmatched,
    }
}
```

Update the imports at the top of `run.rs`:
- add `Confidence` to the `crate::claude` use (it becomes `use crate::claude::{ClaudeMatcher, Confidence, PayeeMatch};`),
- add `use crate::telegram::{ConfirmChannel, Prompt};`,
- add `use crate::matching::best_payee_match;` (the existing `use crate::matching::{Schedule, matches_scheduled};` can be extended to `{Schedule, best_payee_match, matches_scheduled}`),
- extend the state import to `use crate::state::{PendingReview, State};` (`build_prompt` takes `&PendingReview`).

- [ ] **Step 4: Add `build_prompt`**

Add to `src/run.rs` (near the other helpers, e.g. after `payee_id_for`):

```rust
/// Build the Telegram prompt for a held entry. Flavor is chosen by whether a
/// suggestion exists and whether it is a real payee:
/// - suggestion in payee list  -> "raw → suggestion?"  [Confirm][Create new][Skip]
/// - suggestion NOT in payees   -> "No payee 'x'. Create it?" [Create][Skip]
/// - no suggestion              -> "New merchant raw — which payee?" [Create new][Skip]
fn build_prompt(entry: &PendingReview, payee_names: &[String]) -> Prompt {
    use crate::telegram::encode_callback;
    let key = &entry.external_id;
    match &entry.suggestion {
        Some(s) if payee_names.iter().any(|p| p == s) => {
            let conf = entry.confidence.as_deref().unwrap_or("medium");
            Prompt {
                chat_text: format!(
                    "`{}` → *{}*?  _({} confidence)_\nReply with a different payee to remap.",
                    entry.raw_merchant, s, conf
                ),
                buttons: vec![
                    ("✅ Confirm".into(), encode_callback("cf", key)),
                    ("🆕 Create new".into(), encode_callback("nw", key)),
                    ("❌ Skip".into(), encode_callback("sk", key)),
                ],
            }
        }
        Some(s) => Prompt {
            chat_text: format!(
                "No existing payee *{}*. Create it?\nOr reply with a payee name.",
                s
            ),
            buttons: vec![
                ("🆕 Create".into(), encode_callback("nw", key)),
                ("❌ Skip".into(), encode_callback("sk", key)),
            ],
        },
        None => Prompt {
            chat_text: format!(
                "New merchant `{}` — which payee?\nReply with a payee name, or:",
                entry.raw_merchant
            ),
            buttons: vec![
                ("🆕 Create new".into(), encode_callback("nw", key)),
                ("❌ Skip".into(), encode_callback("sk", key)),
            ],
        },
    }
}
```

- [ ] **Step 5: Thread the channel + cutoff through `run` and wire the new behavior**

Change the `run` signature (line 100–109) to add two parameters:

```rust
#[allow(clippy::too_many_arguments)]
pub async fn run(
    cfg: &Config,
    dry_run: bool,
    state: &mut State,
    bank: &mut dyn BankSource,
    actual: &ActualClient,
    claude: &dyn ClaudeMatcher,
    notifier: &dyn Notifier,
    channel: Option<&dyn ConfirmChannel>,
    autoimport: Confidence,
) -> anyhow::Result<()> {
```

After payees/schedules are read (after line 144 `let schedules = ...`), and BEFORE `take_resolved` (line 153), insert the remap-normalisation step:

```rust
    // Normalise any free-text remap replies against the live payee list, so
    // `take_resolved` only ever folds a real existing payee into the cache.
    // A remap that doesn't match becomes a "create it?" hold (suggestion set,
    // resolve_to cleared, prompt re-sent next run).
    let payee_names: Vec<String> = payees.iter().map(|p| p.name.clone()).collect();
    for pr in &mut state.pending_reviews {
        if !pr.create_new && !pr.resolve_to.is_empty() {
            match best_payee_match(&pr.resolve_to, &payee_names, PAYEE_THRESHOLD) {
                Some(canon) => pr.resolve_to = canon,
                None => {
                    pr.suggestion = Some(std::mem::take(&mut pr.resolve_to));
                    pr.telegram_message_id = None;
                }
            }
        }
    }
```

Replace the Claude call block (lines 157–165) so it no longer rebuilds `payee_names` (reuse the one above):

```rust
    let claude_matches: Vec<PayeeMatch> = if plan.needs_claude.is_empty() {
        Vec::new()
    } else {
        claude
            .match_payees(&plan.needs_claude, &payee_names)
            .await
            .context("claude match")?
    };
```

Replace the `apply_matches` call (line 171) and the unmatched-hold loop (lines 251–257). First, change the call:

```rust
    let outcome = apply_matches(&unknown_txs, &claude_matches, autoimport);
```

Then replace the unmatched hold loop with both proposed and unmatched holds:

```rust
    // proposed (medium/low) -> held WITH suggestion + confidence, NOT imported
    for (tx, sugg, conf) in &outcome.proposed {
        let date = tx.date.format("%Y-%m-%d").to_string();
        if state.add_pending_full(
            &tx.external_id,
            &tx.raw_merchant,
            tx.amount_minor,
            &date,
            Some(sugg.clone()),
            Some(conf.as_str().to_string()),
        ) {
            held += 1;
        }
    }
    // unmatched -> held with no suggestion
    for tx in &outcome.unmatched {
        let date = tx.date.format("%Y-%m-%d").to_string();
        if state.add_pending(&tx.external_id, &tx.raw_merchant, tx.amount_minor, &date) {
            held += 1;
        }
    }
```

Update the dry-run report `would hold` line (line 267) to count both:

```rust
        println!(
            "  would hold   : {}",
            outcome.proposed.len() + outcome.unmatched.len()
        );
```

And the dry-run `would hold` listing loop (lines 271–273) — replace with:

```rust
        for (tx, sugg, _) in &outcome.proposed {
            println!("    ? hold {} {} (suggest {})", tx.raw_merchant, tx.amount_minor, sugg);
        }
        for tx in &outcome.unmatched {
            println!("    ? hold {} {}", tx.raw_merchant, tx.amount_minor);
        }
```

- [ ] **Step 6: Send prompts (idempotent) and append the reminder count**

After the live import + `state.last_run` update (after line 308 `state.save(...)`), and BEFORE building the Apprise body, insert the prompt-send step. Replace the block from line 307 (`state.last_run = ...`) down through the notifier call with:

```rust
    state.last_run = Some(Utc::now());

    // Send a prompt for every held entry that hasn't been prompted yet.
    if let Some(ch) = channel {
        // collect indices to avoid borrow conflicts while sending
        let to_prompt: Vec<usize> = state
            .pending_reviews
            .iter()
            .enumerate()
            .filter(|(_, p)| p.telegram_message_id.is_none())
            .map(|(i, _)| i)
            .collect();
        for i in to_prompt {
            let prompt = build_prompt(&state.pending_reviews[i], &payee_names);
            match ch.send_prompt(&prompt).await {
                Ok(message_id) => state.pending_reviews[i].telegram_message_id = Some(message_id),
                Err(e) => warn!("telegram send_prompt failed (will retry next run): {e}"),
            }
        }
    }

    state.save(&cfg.state_path)?;

    let awaiting = state.pending_reviews.len();
    let mut body = format!(
        "Added {}, updated {}, held {} for review.",
        summary.added, summary.updated, held
    );
    if !newly_cached.is_empty() {
        body.push_str(&format!("\nNewly auto-matched ({}):", newly_cached.len()));
        for (raw, payee) in &newly_cached {
            body.push_str(&format!("\n  {raw} -> {payee}"));
        }
    }
    if awaiting > 0 {
        body.push_str(&format!("\n{awaiting} item(s) awaiting your confirmation."));
    }
    notifier.notify("Actual import complete", &body).await.ok();

    Ok(())
}
```

(The previous `state.save` at line 308 is now folded into the block above — ensure there is exactly one `state.save(&cfg.state_path)?;` on the live path, placed after the prompt-send loop so message_ids persist.)

- [ ] **Step 7: Add the `ConfirmChannel` test double (reused by Task 9)**

Add to the `tests` module in `src/run.rs`:

```rust
    use crate::telegram::{ConfirmChannel, Reply};
    use async_trait::async_trait;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// Test double: returns scripted replies once, records sent prompts.
    pub struct FakeChannel {
        pub scripted: Mutex<Vec<Reply>>,
        pub offset: i64,
        pub sent: Mutex<Vec<Prompt>>,
        pub next_id: Mutex<i64>,
    }
    impl FakeChannel {
        pub fn new(scripted: Vec<Reply>, offset: i64) -> Self {
            Self {
                scripted: Mutex::new(scripted),
                offset,
                sent: Mutex::new(Vec::new()),
                next_id: Mutex::new(1000),
            }
        }
    }
    #[async_trait]
    impl ConfirmChannel for FakeChannel {
        async fn poll_replies(
            &self,
            _since: i64,
            _msg_map: &BTreeMap<i64, String>,
        ) -> anyhow::Result<(Vec<Reply>, i64)> {
            Ok((std::mem::take(&mut *self.scripted.lock().unwrap()), self.offset))
        }
        async fn send_prompt(&self, prompt: &Prompt) -> anyhow::Result<i64> {
            self.sent.lock().unwrap().push(prompt.clone());
            let mut id = self.next_id.lock().unwrap();
            *id += 1;
            Ok(*id)
        }
    }
```

- [ ] **Step 8: Run tests, verify they pass**

Run: `cargo test --lib run`
Expected: PASS (partition + prompt-builder tests).

- [ ] **Step 9: Commit**

```bash
git add src/run.rs
git commit -m "feat(run): confidence partition, remap resolution, prompts, reminder"
```

---

## Task 9: `poll` — drain replies into state (write-free)

**Files:**
- Create: `src/poll.rs`
- Modify: `src/main.rs` (add `mod poll;`)

- [ ] **Step 1: Register the module**

In `src/main.rs`, add `mod poll;`.

- [ ] **Step 2: Write the failing test**

Create `src/poll.rs`:

```rust
//! `aba poll`: drain Telegram replies into the state file. Never touches Actual.

use std::collections::BTreeSet;

use tracing::info;

use crate::config::Config;
use crate::state::State;
use crate::telegram::{ConfirmChannel, ReplyAction};

/// Apply scripted/real replies to `state`, advancing the Telegram offset.
/// Returns the number of replies applied.
pub async fn poll(
    cfg: &Config,
    state: &mut State,
    channel: &dyn ConfirmChannel,
) -> anyhow::Result<usize> {
    let msg_map = state
        .pending_reviews
        .iter()
        .filter_map(|p| p.telegram_message_id.map(|m| (m, p.external_id.clone())))
        .collect();
    let (replies, new_offset) = channel.poll_replies(state.telegram.last_update_id, &msg_map).await?;

    let valid: BTreeSet<String> = state
        .pending_reviews
        .iter()
        .map(|p| p.external_id.clone())
        .collect();

    let mut applied = 0usize;
    for r in &replies {
        if !valid.contains(&r.entry_key) {
            continue; // stale / spoofed entry key
        }
        let ok = match &r.action {
            ReplyAction::Confirm => state.confirm_pending(&r.entry_key),
            ReplyAction::CreateNew => state.create_pending(&r.entry_key),
            ReplyAction::Skip => state.skip_pending(&r.entry_key),
            ReplyAction::Remap(text) => state.remap_pending(&r.entry_key, text),
        };
        if ok {
            applied += 1;
        }
    }

    state.telegram.last_update_id = new_offset;
    state.save(&cfg.state_path)?;
    info!("poll applied {applied} repl(ies); offset now {new_offset}");
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::tests::FakeChannel;
    use crate::telegram::Reply;
    use tempfile::tempdir;

    fn cfg_with_state(path: &std::path::Path) -> Config {
        Config::from_getter(|k| match k {
            "ANTHROPIC_API_KEY" => Some("ak".into()),
            "MONZO_CLIENT_ID" => Some("c".into()),
            "MONZO_CLIENT_SECRET" => Some("s".into()),
            "MONZO_REDIRECT_URI" => Some("http://localhost/cb".into()),
            "APPRISE_URL" => Some("http://a/notify".into()),
            "ACTUAL_SERVER_URL" => Some("http://actual".into()),
            "ACTUAL_PASSWORD" => Some("pw".into()),
            "ACTUAL_SYNC_ID" => Some("sync".into()),
            "ABA_STATE_PATH" => Some(path.to_string_lossy().to_string()),
            _ => None,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn applies_confirm_and_ignores_unknown_key() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.toml");
        let cfg = cfg_with_state(&path);

        let mut state = State::default();
        state.add_pending_full("tx1", "SQ *CAFE", -450, "2026-06-01", Some("Local Cafe".into()), Some("medium".into()));
        state.pending_reviews[0].telegram_message_id = Some(500);

        let channel = FakeChannel::new(
            vec![
                Reply { entry_key: "tx1".into(), action: ReplyAction::Confirm },
                Reply { entry_key: "ghost".into(), action: ReplyAction::Skip },
            ],
            123,
        );

        let applied = poll(&cfg, &mut state, &channel).await.unwrap();
        assert_eq!(applied, 1);
        assert_eq!(state.pending_reviews[0].resolve_to, "Local Cafe");
        assert_eq!(state.telegram.last_update_id, 123);

        // persisted
        let reloaded = State::load(&path).unwrap();
        assert_eq!(reloaded.telegram.last_update_id, 123);
        assert_eq!(reloaded.pending_reviews[0].resolve_to, "Local Cafe");
    }
}
```

Note: this test imports `crate::run::tests::FakeChannel`, so the `tests` module in `run.rs` must be visible to the crate. Change `run.rs`'s test module declaration to `pub(crate) mod tests {` (it is currently `mod tests`). The `FakeChannel` struct is already `pub` (Task 8 step 7).

- [ ] **Step 3: Run test, verify it fails then passes**

Run: `cargo test --lib poll`
Expected: initially FAIL if `run::tests` is private; after the `pub(crate) mod tests` change in `run.rs`, PASS.

- [ ] **Step 4: Commit**

```bash
git add src/poll.rs src/main.rs src/run.rs
git commit -m "feat(poll): drain telegram replies into state (write-free)"
```

---

## Task 10: `run` integration test — async-across-runs + idempotent prompts + dry-run

**Files:**
- Modify: `src/run.rs`

This proves the headline guarantee: a reply applied by `poll` is imported by the next `run`; prompts are sent once; dry-run sends nothing. It uses lightweight in-process doubles for `BankSource`/`ClaudeMatcher`/`Notifier`, the `FakeChannel` from Task 8, and a fake-`node` shim (a tempdir `index.js` that branches on the request `op` and records imports to a file in its own directory). Requires `node` on PATH — same as the existing `src/actual/mod.rs` tests.

- [ ] **Step 1: Add the in-process doubles and a fake-shim builder**

Add to the `tests` module in `src/run.rs`:

```rust
    use crate::actual::ActualClient;
    use crate::banks::{BankError, BankSource, RawTransaction};
    use crate::claude::ClaudeMatcher;
    use crate::notify::Notifier;
    use chrono::{DateTime, Utc};
    use tempfile::TempDir;

    struct FakeBank { txs: Vec<RawTransaction> }
    #[async_trait]
    impl BankSource for FakeBank {
        fn name(&self) -> &'static str { "fake" }
        async fn ensure_auth(&mut self, _s: &mut State) -> Result<(), BankError> { Ok(()) }
        async fn fetch_since(&self, _since: DateTime<Utc>) -> Result<Vec<RawTransaction>, BankError> {
            Ok(self.txs.clone())
        }
    }

    struct FakeClaude { out: Vec<PayeeMatch> }
    #[async_trait]
    impl ClaudeMatcher for FakeClaude {
        async fn match_payees(&self, _raw: &[String], _p: &[String]) -> anyhow::Result<Vec<PayeeMatch>> {
            Ok(self.out.clone())
        }
    }

    struct NullNotifier;
    #[async_trait]
    impl Notifier for NullNotifier {
        async fn notify(&self, _t: &str, _b: &str) -> anyhow::Result<()> { Ok(()) }
    }

    /// Build an `ActualClient` backed by a fake node shim in `dir`. `get_payees`
    /// returns `[{p_cafe, Local Cafe}]`, `get_scheduled` is empty, `create_payee`
    /// returns `{id: p_new}`, and `import_transactions` appends the imported rows
    /// to `<dir>/imports.log` (one JSON array per call) so the test can assert.
    fn fake_actual(dir: &TempDir) -> ActualClient {
        let script = r#"
let b='';process.stdin.on('data',c=>b+=c);process.stdin.on('end',()=>{
  const req=JSON.parse(b); const out=o=>process.stdout.write(JSON.stringify(o));
  const fs=require('fs'), path=require('path');
  if(req.op==='get_payees'){ out({ok:true,data:[{id:'p_cafe',name:'Local Cafe'}]}); }
  else if(req.op==='get_scheduled'){ out({ok:true,data:[]}); }
  else if(req.op==='create_payee'){ out({ok:true,data:{id:'p_new'}}); }
  else if(req.op==='import_transactions'){
    fs.appendFileSync(path.join(__dirname,'imports.log'), JSON.stringify(req.transactions)+'\n');
    out({ok:true,data:{added:req.transactions.length,updated:0}});
  } else { out({ok:false,error:'unknown op'}); }
});
"#;
        std::fs::write(dir.path().join("index.js"), script).unwrap();
        ActualClient::new(dir.path().to_path_buf(), false, "http://t".into(), "pw".into(), "sync".into())
    }

    fn cfg_at(state_path: &std::path::Path) -> Config {
        Config::from_getter(|k| match k {
            "ANTHROPIC_API_KEY" => Some("ak".into()),
            "MONZO_CLIENT_ID" => Some("c".into()),
            "MONZO_CLIENT_SECRET" => Some("s".into()),
            "MONZO_REDIRECT_URI" => Some("http://localhost/cb".into()),
            "APPRISE_URL" => Some("http://a/notify".into()),
            "ACTUAL_SERVER_URL" => Some("http://actual".into()),
            "ACTUAL_PASSWORD" => Some("pw".into()),
            "ACTUAL_SYNC_ID" => Some("sync".into()),
            "ABA_STATE_PATH" => Some(state_path.to_string_lossy().to_string()),
            _ => None,
        }).unwrap()
    }
```

- [ ] **Step 2: Write the async-across-runs integration test**

Add to the `tests` module in `src/run.rs`:

```rust
    #[tokio::test]
    async fn medium_match_held_prompted_then_imported_after_confirm() {
        let shim = tempfile::tempdir().unwrap();
        let statedir = tempfile::tempdir().unwrap();
        let state_path = statedir.path().join("s.toml");
        let imports_log = shim.path().join("imports.log");
        let cfg = cfg_at(&state_path);
        let actual = fake_actual(&shim);
        let notifier = NullNotifier;
        let channel = FakeChannel::new(vec![], 0);

        // ---- Run #1: one medium-confidence match -> held + prompted, NOT imported.
        let mut state = State::default();
        let mut bank = FakeBank { txs: vec![tx("tx_cafe", "SQ *CAFE", -450)] };
        let claude = FakeClaude { out: vec![PayeeMatch {
            raw: "SQ *CAFE".into(), payee: Some("Local Cafe".into()), confidence: Confidence::Medium,
        }]};
        run(&cfg, false, &mut state, &mut bank, &actual, &claude, &notifier,
            Some(&channel as &dyn ConfirmChannel), Confidence::High).await.unwrap();

        assert!(!imports_log.exists(), "nothing should be imported yet");
        assert_eq!(state.pending_reviews.len(), 1);
        assert_eq!(state.pending_reviews[0].suggestion.as_deref(), Some("Local Cafe"));
        assert!(state.pending_reviews[0].telegram_message_id.is_some());
        assert_eq!(channel.sent.lock().unwrap().len(), 1);

        // ---- User confirms (what `poll` would do).
        assert!(state.confirm_pending("tx_cafe"));

        // ---- Run #2: no new txs -> the held tx is now imported, prompt NOT re-sent.
        let mut bank2 = FakeBank { txs: vec![] };
        let claude2 = FakeClaude { out: vec![] };
        run(&cfg, false, &mut state, &mut bank2, &actual, &claude2, &notifier,
            Some(&channel as &dyn ConfirmChannel), Confidence::High).await.unwrap();

        let logged = std::fs::read_to_string(&imports_log).unwrap();
        assert!(logged.contains("tx_cafe"), "held tx imported under imported_id");
        assert!(logged.contains("p_cafe"), "imported under the Local Cafe payee id");
        assert!(state.pending_reviews.is_empty(), "resolved entry removed");
        assert_eq!(channel.sent.lock().unwrap().len(), 1, "no re-prompt (idempotent)");
    }

    #[tokio::test]
    async fn dry_run_sends_no_prompts_and_no_writes() {
        let shim = tempfile::tempdir().unwrap();
        let statedir = tempfile::tempdir().unwrap();
        let state_path = statedir.path().join("s.toml");
        let imports_log = shim.path().join("imports.log");
        let cfg = cfg_at(&state_path);
        let actual = fake_actual(&shim);
        let notifier = NullNotifier;
        let channel = FakeChannel::new(vec![], 0);

        let mut state = State::default();
        let mut bank = FakeBank { txs: vec![tx("tx_cafe", "SQ *CAFE", -450)] };
        let claude = FakeClaude { out: vec![PayeeMatch {
            raw: "SQ *CAFE".into(), payee: Some("Local Cafe".into()), confidence: Confidence::Medium,
        }]};
        run(&cfg, true, &mut state, &mut bank, &actual, &claude, &notifier,
            Some(&channel as &dyn ConfirmChannel), Confidence::High).await.unwrap();

        assert!(channel.sent.lock().unwrap().is_empty(), "dry-run sends no prompts");
        assert!(!imports_log.exists(), "dry-run writes nothing to Actual");
        let on_disk = State::load(&state_path).unwrap();
        assert!(on_disk.pending_reviews.is_empty(), "dry-run does not persist holds");
        assert_eq!(on_disk.last_run, None, "dry-run does not advance last_run");
    }
```

- [ ] **Step 3: Run the integration tests, verify they pass**

Run: `cargo test --lib run::tests::medium_match_held_prompted_then_imported_after_confirm run::tests::dry_run_sends_no_prompts_and_no_writes`
Expected: PASS. (Requires `node` on PATH, like the existing `actual` tests. The fake shim writes `imports.log` into its own tempdir, so no env-var plumbing is needed.)

- [ ] **Step 4: Commit**

```bash
git add src/run.rs
git commit -m "test(run): async-across-runs confirm->import, idempotent prompts, dry-run"
```

---

## Task 11: Wire `main.rs` — channel construction, dispatch, file locks

**Files:**
- Modify: `src/main.rs`

- [ ] **Step 1: Add imports and a channel builder**

In `src/main.rs`, add imports:

```rust
use crate::claude::Confidence;
use crate::config::Command;
use crate::poll::poll;
use crate::telegram::HttpTelegram;
use crate::lock::{lock_blocking, lock_try};
```

Add a helper that builds the channel only when both Telegram vars are present:

```rust
fn build_channel(cfg: &Config) -> Option<HttpTelegram> {
    match (cfg.telegram_bot_token.as_ref(), cfg.telegram_chat_id.as_ref()) {
        (Some(tok), Some(chat)) => match chat.parse::<i64>() {
            Ok(chat_id) => Some(HttpTelegram::new(tok.clone(), chat_id)),
            Err(_) => {
                tracing::warn!("TELEGRAM_CHAT_ID is not a valid integer; Telegram disabled");
                None
            }
        },
        _ => None,
    }
}
```

- [ ] **Step 2: Update the `Run` arm to take a blocking lock and pass channel + cutoff**

Replace the `Command::Run { dry_run } => { ... }` body so it acquires the lock first and passes the new args. The lock guard (`_lock`) must live until the end of the arm:

```rust
        Command::Run { dry_run } => {
            let _lock = lock_blocking(&cfg.lock_path)?;
            let mut state = State::load(&cfg.state_path)?;
            let mut bank = Monzo::new(
                cfg.monzo_client_id.clone(),
                cfg.monzo_client_secret.clone(),
                cfg.monzo_base_url.clone(),
                state.monzo_clone(),
            );
            let actual = ActualClient::new(
                cfg.shim_dir.clone(),
                dry_run,
                cfg.actual_server_url.clone(),
                cfg.actual_password.clone(),
                cfg.actual_sync_id.clone(),
            );
            let claude = HttpClaude::new(cfg.anthropic_api_key.clone(), cfg.claude_model.clone());
            let notifier = HttpApprise::new(cfg.apprise_url.clone());
            let channel = build_channel(&cfg);
            let autoimport = Confidence::parse(&cfg.autoimport_confidence);

            run::run(
                &cfg,
                dry_run,
                &mut state,
                &mut bank,
                &actual,
                &claude,
                &notifier,
                channel.as_ref().map(|c| c as &dyn crate::telegram::ConfirmChannel),
                autoimport,
            )
            .await
        }
```

- [ ] **Step 3: Add the `Poll` arm**

Add to the `match cli.command`:

```rust
        Command::Poll => {
            let Some(channel) = build_channel(&cfg) else {
                tracing::info!("Telegram not configured; poll is a no-op");
                return Ok(());
            };
            // Try-lock: if `run` (or another poll) holds it, skip this tick.
            let Some(_lock) = lock_try(&cfg.lock_path)? else {
                tracing::info!("state locked by another process; skipping this poll");
                return Ok(());
            };
            let mut state = State::load(&cfg.state_path)?;
            poll(&cfg, &mut state, &channel).await?;
            Ok(())
        }
```

- [ ] **Step 4: Build and run the whole suite**

Run: `cargo build`
Expected: compiles. Then `cargo test`.
Expected: PASS (all tests).

- [ ] **Step 5: Commit**

```bash
git add src/main.rs
git commit -m "feat(main): wire poll subcommand, telegram channel, file locks"
```

---

## Task 12: Docs + final gates

**Files:**
- Modify: `docs/RUNBOOK.md`
- Modify: `docs/superpowers/specs/2026-06-01-telegram-confirm-loop-design.md` (align with two small impl decisions)

- [ ] **Step 1: Update the RUNBOOK**

Add a "Telegram confirm loop" section documenting:
- The two cron lines:
  ```cron
  */15 * * * *  docker run --rm --network <net> -v /srv/aba:/data <image> poll
  0   3 * * *   docker run --rm --network <net> -v /srv/aba:/data <image> run
  ```
- New env vars: `TELEGRAM_BOT_TOKEN`, `TELEGRAM_CHAT_ID`, `ABA_AUTOIMPORT_CONFIDENCE` (default `high`), `ABA_LOCK_PATH` (default `<state>.lock`), `TELEGRAM_BASE_URL` (default `https://api.telegram.org`).
- Bot setup: create a bot via BotFather, copy the token; send the bot any message; read your numeric `chat_id` once from `https://api.telegram.org/bot<token>/getUpdates`; set both env vars.
- Fire-exit note: TOML `resolve_to`/`create_new` still works if Telegram is down; if Telegram is unconfigured, items are held and resolvable by file edit.

- [ ] **Step 2: Align the spec with two implementation decisions**

Edit the spec so it matches the built behavior:
- `ConfirmChannel::send_prompt` takes a prepared `Prompt` (built by `run`), and `poll_replies` takes a `msg_map` for reply correlation (the spec's trait sketch showed `send_prompt(&PendingReview, &[payees])`).
- Free-text remap that does not fuzzy-match a payee is re-held with `suggestion = <typed text>` and re-prompted as a **"create it?"** flavor (Create / Skip), driven by `build_prompt` keying on payee-list membership.

- [ ] **Step 3: Run the full quality gate**

Run each and confirm clean (do not infer success from a piped tail — read each command's own exit/summary):

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Expected: fmt clean, clippy zero warnings, all tests PASS.

- [ ] **Step 4: Commit**

```bash
git add docs/RUNBOOK.md docs/superpowers/specs/2026-06-01-telegram-confirm-loop-design.md
git commit -m "docs: telegram confirm loop runbook + spec alignment"
```

---

## Self-review checklist (run before handoff)

- **Spec coverage:** confidence enum (T1), held suggestions + offset + mutators (T2), remap fuzzy match (T3), Telegram trait/transport/parsing (T4–5), file lock (T6), config + poll command (T7), confidence partition + remap normalisation + prompts + reminder (T8), poll drain (T9), async-across-runs + idempotency + dry-run guarantees (T10), wiring + locks (T11), docs + chat_id allowlist verified in T4/T5 parsing tests, fire-exit fallback in T11 (`build_channel` None path) (T12). ✅
- **chat_id allowlist:** enforced in `parse_get_updates` (T4 test `..._foreign_chat`) and exercised through `HttpTelegram` (T5). ✅
- **entry_key validation:** enforced in `poll` (T9 test `..._ignores_unknown_key`). ✅
- **No silent auto-create:** create only via explicit `create_new` (Confirm never creates; failed remap → "create it?" requires a tap). ✅
- **Dry-run:** no Telegram sends, no Actual writes, only rotated token persists (T10 `dry_run_sends_no_prompts`). ✅
- **Type consistency:** `Confidence`, `MatchOutcome{matched,proposed,unmatched}`, `apply_matches(.., Confidence)`, `Prompt{chat_text,buttons}`, `Reply{entry_key,action}`, `ConfirmChannel::{poll_replies(since,msg_map), send_prompt(prompt)}`, state mutators `confirm/remap/create/skip_pending`, `add_pending_full` — names match across T1–T11. ✅
```
