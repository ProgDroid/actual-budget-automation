//! Persistent TOML state.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub last_run: Option<DateTime<Utc>>,
    #[serde(default)]
    pub monzo: MonzoState,
    /// raw merchant -> resolved Actual payee name.
    #[serde(default)]
    pub payee_cache: BTreeMap<String, String>,
    #[serde(default, rename = "pending_review")]
    pub pending_reviews: Vec<PendingReview>,
    #[serde(default)]
    pub telegram: TelegramState,
}

#[derive(Default, PartialEq, Serialize, Deserialize)]
pub struct MonzoState {
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub account_id: String,
}

impl std::fmt::Debug for MonzoState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MonzoState")
            .field(
                "refresh_token",
                &if self.refresh_token.is_empty() { "<empty>" } else { "***" },
            )
            .field("account_id", &self.account_id)
            .finish()
    }
}

#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TelegramState {
    /// Highest acknowledged getUpdates offset; advanced only by `aba poll`.
    #[serde(default)]
    pub last_update_id: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingReview {
    #[serde(default)]
    pub external_id: String,
    pub raw_merchant: String,
    pub amount: i64,
    pub date: String,
    /// Set to an existing payee name to map to it.
    #[serde(default)]
    pub resolve_to: String,
    /// Set true to create a new payee (named `resolve_to` if given, else `raw_merchant`).
    #[serde(default)]
    pub create_new: bool,
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
}

impl State {
    pub fn load(path: &Path) -> anyhow::Result<State> {
        // path comes from CLI config, not from untrusted user input
        let read_result = std::fs::read_to_string(path); // nosemgrep: rust.actix.path-traversal.tainted-path.tainted-path
        match read_result {
            Ok(s) => Ok(toml::from_str(&s)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Atomic write: write to a temp sibling then rename.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let body = toml::to_string_pretty(self)?;
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Remove resolved pending reviews, fold their raw->payee mapping into the
    /// cache, and return them for import. Unresolved entries are retained.
    pub fn take_resolved(&mut self) -> Vec<ResolvedReview> {
        let mut resolved = Vec::new();
        let mut remaining = Vec::new();
        for pr in std::mem::take(&mut self.pending_reviews) {
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
                (pr.resolve_to.clone(), false)
            } else {
                remaining.push(pr);
                continue;
            };
            self.payee_cache
                .insert(pr.raw_merchant.clone(), payee_name.clone());
            resolved.push(ResolvedReview {
                external_id: pr.external_id,
                raw_merchant: pr.raw_merchant,
                amount: pr.amount,
                date: pr.date,
                payee_name,
                create_new,
            });
        }
        self.pending_reviews = remaining;
        resolved
    }

    /// Clone the Monzo sub-state (for constructing the bank source).
    pub fn monzo_clone(&self) -> MonzoState {
        MonzoState {
            refresh_token: self.monzo.refresh_token.clone(),
            account_id: self.monzo.account_id.clone(),
        }
    }

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
}

/// A pending review the user has resolved, ready to import.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedReview {
    pub external_id: String,
    pub raw_merchant: String,
    pub amount: i64,
    pub date: String,
    /// Final payee name to use.
    pub payee_name: String,
    /// True if the payee must be created in Actual first.
    pub create_new: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn load_missing_file_returns_default() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("missing.state.toml");
        let s = State::load(&path).unwrap();
        assert_eq!(s, State::default());
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.state.toml");
        let mut s = State::default();
        s.monzo.refresh_token = "rt_123".into();
        s.monzo.account_id = "acc_1".into();
        s.payee_cache
            .insert("TFL TRAVEL CH".into(), "Transport for London".into());
        s.pending_reviews.push(PendingReview {
            external_id: "tx_cafe_save".into(),
            raw_merchant: "SQ *SOME CAFE".into(),
            amount: -450,
            date: "2026-05-30".into(),
            resolve_to: String::new(),
            create_new: false,
            suggestion: None,
            confidence: None,
            telegram_message_id: None,
        });
        s.save(&path).unwrap();
        let loaded = State::load(&path).unwrap();
        assert_eq!(loaded, s);
    }

    #[test]
    fn take_resolved_maps_existing_and_creates_and_keeps_unresolved() {
        let mut s = State::default();
        // resolved to existing payee
        s.pending_reviews.push(PendingReview {
            external_id: "tx_tfl".into(),
            raw_merchant: "TFL TRAVEL CH".into(),
            amount: -275,
            date: "2026-05-30".into(),
            resolve_to: "Transport for London".into(),
            create_new: false,
            suggestion: None,
            confidence: None,
            telegram_message_id: None,
        });
        // create new under explicit name
        s.pending_reviews.push(PendingReview {
            external_id: "tx_cafe".into(),
            raw_merchant: "SQ *SOME CAFE".into(),
            amount: -450,
            date: "2026-05-30".into(),
            resolve_to: "Some Cafe".into(),
            create_new: true,
            suggestion: None,
            confidence: None,
            telegram_message_id: None,
        });
        // unresolved -> stays
        s.pending_reviews.push(PendingReview {
            external_id: "tx_amzn".into(),
            raw_merchant: "AMZNMKTPLACE".into(),
            amount: -1999,
            date: "2026-05-31".into(),
            resolve_to: String::new(),
            create_new: false,
            suggestion: None,
            confidence: None,
            telegram_message_id: None,
        });

        let resolved = s.take_resolved();

        assert_eq!(resolved.len(), 2);
        assert_eq!(s.pending_reviews.len(), 1);
        assert_eq!(s.pending_reviews[0].raw_merchant, "AMZNMKTPLACE");
        // cache updated for the two resolved
        assert_eq!(
            s.payee_cache.get("TFL TRAVEL CH").unwrap(),
            "Transport for London"
        );
        assert_eq!(s.payee_cache.get("SQ *SOME CAFE").unwrap(), "Some Cafe");
        let cafe = resolved
            .iter()
            .find(|r| r.raw_merchant == "SQ *SOME CAFE")
            .unwrap();
        assert!(cafe.create_new);
        assert_eq!(cafe.payee_name, "Some Cafe");
    }

    #[test]
    fn add_pending_dedupes_on_identity() {
        let mut s = State::default();
        let added1 = s.add_pending("tx1", "AMZNMKTPLACE", -1999, "2026-05-31");
        let added2 = s.add_pending("tx1", "AMZNMKTPLACE", -1999, "2026-05-31");
        assert!(added1);
        assert!(!added2);
        assert_eq!(s.pending_reviews.len(), 1);
    }

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
    fn monzo_state_debug_redacts_refresh_token() {
        let m = MonzoState { refresh_token: "SECRET_RT".into(), account_id: "acc_1".into() };
        let s = format!("{m:?}");
        assert!(!s.contains("SECRET_RT"), "refresh_token must be redacted: {s}");
        assert!(s.contains("acc_1"), "account_id is fine to show");
    }

    #[test]
    fn reply_mutators_apply() {
        let mut s = State::default();
        s.add_pending_full(
            "a",
            "AAA",
            -1,
            "2026-06-01",
            Some("Aldi".into()),
            Some("medium".into()),
        );
        s.add_pending_full("b", "BBB", -2, "2026-06-01", None, None);
        s.add_pending_full(
            "c",
            "CCC",
            -3,
            "2026-06-01",
            Some("Costa".into()),
            Some("low".into()),
        );
        s.add_pending_full("d", "DDD", -4, "2026-06-01", None, None);
        // dedicated no-suggestion entry: confirm must refuse to set resolve_to.
        s.add_pending_full("e", "EEE", -5, "2026-06-01", None, None);

        // confirm on a None-suggestion entry is a no-op returning false.
        assert!(!s.confirm_pending("e"));
        assert!(
            s.pending_reviews
                .iter()
                .find(|p| p.external_id == "e")
                .unwrap()
                .resolve_to
                .is_empty()
        );

        assert!(s.confirm_pending("a")); // resolve_to <- suggestion
        assert!(s.remap_pending("b", "Boots")); // resolve_to <- free text
        assert!(s.create_pending("c")); // create_new = true
        assert!(s.skip_pending("d")); // removed
        assert!(!s.confirm_pending("missing"));
        assert!(!s.skip_pending("does_not_exist")); // unknown id -> false

        let by = |id: &str| s.pending_reviews.iter().find(|p| p.external_id == id);
        assert_eq!(by("a").unwrap().resolve_to, "Aldi");
        assert_eq!(by("b").unwrap().resolve_to, "Boots");
        assert!(by("c").unwrap().create_new);
        assert!(by("d").is_none());
    }
}
