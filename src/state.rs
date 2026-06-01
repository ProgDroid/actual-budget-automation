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
}
