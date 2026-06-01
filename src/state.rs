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
}

#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MonzoState {
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub account_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingReview {
    pub raw_merchant: String,
    pub amount: i64,
    pub date: String,
    /// Set to an existing payee name to map to it.
    #[serde(default)]
    pub resolve_to: String,
    /// Set true to create a new payee (named `resolve_to` if given, else `raw_merchant`).
    #[serde(default)]
    pub create_new: bool,
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
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
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
                let name = if pr.resolve_to.is_empty() {
                    pr.raw_merchant.clone()
                } else {
                    pr.resolve_to.clone()
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

    /// Add a pending review unless an identical (merchant, amount, date) entry
    /// already exists. Returns true if added.
    pub fn add_pending(&mut self, raw_merchant: &str, amount: i64, date: &str) -> bool {
        let exists = self
            .pending_reviews
            .iter()
            .any(|p| p.raw_merchant == raw_merchant && p.amount == amount && p.date == date);
        if exists {
            return false;
        }
        self.pending_reviews.push(PendingReview {
            raw_merchant: raw_merchant.to_string(),
            amount,
            date: date.to_string(),
            resolve_to: String::new(),
            create_new: false,
        });
        true
    }
}

/// A pending review the user has resolved, ready to import.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedReview {
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
            raw_merchant: "SQ *SOME CAFE".into(),
            amount: -450,
            date: "2026-05-30".into(),
            resolve_to: String::new(),
            create_new: false,
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
            raw_merchant: "TFL TRAVEL CH".into(),
            amount: -275,
            date: "2026-05-30".into(),
            resolve_to: "Transport for London".into(),
            create_new: false,
        });
        // create new under explicit name
        s.pending_reviews.push(PendingReview {
            raw_merchant: "SQ *SOME CAFE".into(),
            amount: -450,
            date: "2026-05-30".into(),
            resolve_to: "Some Cafe".into(),
            create_new: true,
        });
        // unresolved -> stays
        s.pending_reviews.push(PendingReview {
            raw_merchant: "AMZNMKTPLACE".into(),
            amount: -1999,
            date: "2026-05-31".into(),
            resolve_to: String::new(),
            create_new: false,
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
        let added1 = s.add_pending("AMZNMKTPLACE", -1999, "2026-05-31");
        let added2 = s.add_pending("AMZNMKTPLACE", -1999, "2026-05-31");
        assert!(added1);
        assert!(!added2);
        assert_eq!(s.pending_reviews.len(), 1);
    }
}
