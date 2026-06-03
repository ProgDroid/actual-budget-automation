//! Persistent TOML state.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub last_run: Option<DateTime<Utc>>,
    #[serde(default)]
    pub monzo: MonzoState,
    /// raw merchant -> resolved Actual payee name.
    #[serde(default)]
    pub payee_cache: BTreeMap<String, String>,
    /// raw merchant -> resolved Actual category name.
    #[serde(default)]
    pub category_cache: BTreeMap<String, String>,
    #[serde(default, rename = "pending_review")]
    pub pending_reviews: Vec<PendingReview>,
    #[serde(default)]
    pub telegram: TelegramState,
}

#[derive(Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonzoState {
    #[serde(default)]
    pub refresh_token: String,
    /// Legacy single Monzo account id. Retained ONLY so old state files still
    /// deserialise; migrated into `accounts` by `State::load`. Do not read directly.
    #[serde(default)]
    pub account_id: String,
    /// Monzo→Actual account links. Empty until `aba accounts` populates it.
    #[serde(default)]
    pub accounts: Vec<AccountLink>,
}

/// One Monzo account mapped to one Actual account.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountLink {
    /// Monzo account id (`acc_…`) — used for fetching.
    pub monzo_id: String,
    /// Monzo display name (suggest/review UX only).
    #[serde(default)]
    pub monzo_name: String,
    /// Actual account UUID — the import target. Empty = unmapped.
    #[serde(default)]
    pub actual_id: String,
    /// Actual account name (suggest/review UX only).
    #[serde(default)]
    pub actual_name: String,
}

impl std::fmt::Debug for MonzoState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MonzoState")
            .field(
                "refresh_token",
                &if self.refresh_token.is_empty() {
                    "<empty>"
                } else {
                    "***"
                },
            )
            .field("account_id", &self.account_id)
            .field("accounts", &self.accounts)
            .finish()
    }
}

#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelegramState {
    /// Highest acknowledged getUpdates offset; advanced only by `aba poll`.
    #[serde(default)]
    pub last_update_id: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingReview {
    #[serde(default)]
    pub external_id: String,
    /// Monzo account this held txn came from; routes the import once resolved.
    #[serde(default)]
    pub source_monzo_id: String,
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
    /// Claude's proposed category for a medium/low-confidence held category.
    /// `None` = no category proposal (genuine no-match or not applicable).
    #[serde(default)]
    pub category_suggestion: Option<String>,
    /// "medium" | "low" when this was a confidence-held category proposal.
    #[serde(default)]
    pub category_confidence: Option<String>,
    /// Set to an existing category name to map to it. Empty = uncategorised.
    #[serde(default)]
    pub resolve_category_to: String,
    /// `true` once the category dimension is settled (confirmed / remapped /
    /// skipped) OR when no category question applies (set at construction). A
    /// held entry awaiting a category decision has this `false`. The txn imports
    /// only when this is true AND the payee dimension is settled.
    #[serde(default)]
    pub category_resolved: bool,
    /// `Some` once a Telegram prompt has been sent (idempotency guard).
    #[serde(default)]
    pub telegram_message_id: Option<i64>,
}

impl State {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        // path comes from CLI config, not from untrusted user input
        let read_result = std::fs::read_to_string(path); // nosemgrep: rust.actix.path-traversal.tainted-path.tainted-path
        match read_result {
            Ok(s) => {
                let mut state: Self = toml::from_str(&s)?;
                state.migrate_legacy_account();
                Ok(state)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// One-time migration: an old state file has a single `monzo.account_id`
    /// (the Monzo id) and no `accounts`. Promote it to a single UNMAPPED link —
    /// `actual_id` is left empty because the legacy value was the Monzo side
    /// only; the user supplies the Actual side via `aba accounts`.
    fn migrate_legacy_account(&mut self) {
        if self.monzo.accounts.is_empty() && !self.monzo.account_id.is_empty() {
            self.monzo.accounts.push(AccountLink {
                monzo_id: std::mem::take(&mut self.monzo.account_id),
                ..AccountLink::default()
            });
        }
    }

    /// Atomic write: write to a temp file in the target directory, then persist
    /// (rename) over the destination. Same-dir temp avoids cross-filesystem
    /// EXDEV rename failures.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        use std::io::Write as _;
        let body = toml::to_string_pretty(self)?;
        let dir = match path.parent().filter(|p| !p.as_os_str().is_empty()) {
            Some(p) => {
                std::fs::create_dir_all(p)?;
                p.to_path_buf()
            }
            None => std::path::PathBuf::from("."),
        };
        let mut tmp = tempfile::NamedTempFile::new_in(&dir)?;
        tmp.write_all(body.as_bytes())?;
        tmp.flush()?;
        tmp.persist(path).map_err(|e| e.error)?;
        Ok(())
    }

    /// Remove fully-resolved pending reviews (both payee AND category dimensions
    /// settled), fold their raw->payee and raw->category mappings into the caches,
    /// and return them for import. Entries with either dimension unsettled remain.
    pub fn take_resolved(&mut self) -> Vec<ResolvedReview> {
        let mut resolved = Vec::new();
        let mut remaining = Vec::new();
        for pr in std::mem::take(&mut self.pending_reviews) {
            // Payee dimension (unchanged logic).
            let payee = if pr.create_new {
                let name = if !pr.resolve_to.is_empty() {
                    pr.resolve_to.clone()
                } else if let Some(s) = &pr.suggestion {
                    s.clone()
                } else {
                    pr.raw_merchant.clone()
                };
                Some((name, true))
            } else if !pr.resolve_to.is_empty() {
                Some((pr.resolve_to.clone(), false))
            } else {
                None
            };
            // Both dimensions must be settled to import.
            let Some((payee_name, create_new)) = payee else {
                remaining.push(pr);
                continue;
            };
            if !pr.category_resolved {
                remaining.push(pr);
                continue;
            }
            // Category name: empty resolve target = uncategorised (None).
            let category_name = if pr.resolve_category_to.is_empty() {
                None
            } else {
                Some(pr.resolve_category_to.clone())
            };
            self.payee_cache
                .insert(pr.raw_merchant.clone(), payee_name.clone());
            if let Some(cat) = &category_name {
                self.category_cache
                    .insert(pr.raw_merchant.clone(), cat.clone());
            }
            resolved.push(ResolvedReview {
                external_id: pr.external_id,
                raw_merchant: pr.raw_merchant,
                amount: pr.amount,
                date: pr.date,
                payee_name,
                create_new,
                category_name,
                source_monzo_id: pr.source_monzo_id,
            });
        }
        self.pending_reviews = remaining;
        resolved
    }

    /// Clone the Monzo sub-state (for constructing the bank source).
    pub fn monzo_clone(&self) -> MonzoState {
        MonzoState {
            refresh_token: self.monzo.refresh_token.clone(),
            account_id: String::new(),
            accounts: self.monzo.accounts.clone(),
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
        source_monzo_id: &str,
    ) -> bool {
        self.add_pending_full(
            external_id,
            raw_merchant,
            amount,
            date,
            None,
            None,
            source_monzo_id,
        )
    }

    /// Add a pending review with an optional Claude suggestion + confidence.
    #[allow(clippy::too_many_arguments)]
    pub fn add_pending_full(
        &mut self,
        external_id: &str,
        raw_merchant: &str,
        amount: i64,
        date: &str,
        suggestion: Option<String>,
        confidence: Option<String>,
        source_monzo_id: &str,
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
            source_monzo_id: source_monzo_id.to_string(),
            raw_merchant: raw_merchant.to_string(),
            amount,
            date: date.to_string(),
            resolve_to: String::new(),
            create_new: false,
            suggestion,
            confidence,
            category_suggestion: None,
            category_confidence: None,
            resolve_category_to: String::new(),
            category_resolved: true,
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

    /// Confirm the held entry's suggested category (`category_suggestion` -> resolve target).
    /// No-op returning `false` when there is no `category_suggestion` to confirm
    /// (mirrors `confirm_pending`).
    // Called from poll.rs (Task 6); suppress dead_code until that task lands.
    #[allow(dead_code)]
    pub fn confirm_category(&mut self, external_id: &str) -> bool {
        if let Some(pr) = self
            .pending_reviews
            .iter_mut()
            .find(|p| p.external_id == external_id)
            && let Some(s) = pr.category_suggestion.clone()
        {
            pr.resolve_category_to = s;
            pr.category_resolved = true;
            return true;
        }
        false
    }

    /// Remap the held entry's category to a user-supplied name.
    // Called from poll.rs (Task 6); suppress dead_code until that task lands.
    #[allow(dead_code)]
    pub fn remap_category(&mut self, external_id: &str, name: &str) -> bool {
        if let Some(pr) = self
            .pending_reviews
            .iter_mut()
            .find(|p| p.external_id == external_id)
        {
            pr.resolve_category_to = name.to_string();
            pr.category_resolved = true;
            true
        } else {
            false
        }
    }

    /// Skip the category — import uncategorised (settles the dimension).
    // Called from poll.rs (Task 6); suppress dead_code until that task lands.
    #[allow(dead_code)]
    pub fn skip_category(&mut self, external_id: &str) -> bool {
        if let Some(pr) = self
            .pending_reviews
            .iter_mut()
            .find(|p| p.external_id == external_id)
        {
            pr.resolve_category_to = String::new();
            pr.category_resolved = true;
            true
        } else {
            false
        }
    }

    /// Add a held review carrying both payee and category dimension state.
    /// A `Some(name)` in `hold.payee_resolved`/`hold.category_resolved`
    /// PRE-SETTLES that dimension (confident & exists -> no prompt); `None`
    /// leaves it unsettled so the corresponding suggestion drives a review
    /// prompt. The named `ClassifiedHold` (rather than a flat list of six
    /// same-typed `Option<String>` params, or the run-side `HeldClassification`)
    /// prevents silent field transposition and keeps the state→run dependency
    /// direction correct. Returns false (no-op) if an entry with the same
    /// `external_id` already exists.
    pub fn add_pending_classified(
        &mut self,
        external_id: &str,
        raw_merchant: &str,
        amount: i64,
        date: &str,
        source_monzo_id: &str,
        hold: ClassifiedHold,
    ) -> bool {
        if self
            .pending_reviews
            .iter()
            .any(|p| p.external_id == external_id)
        {
            return false;
        }
        // Pre-settled payee -> resolve_to (create_new stays false: we only
        // pre-settle to an EXISTING payee). Unsettled -> empty resolve_to.
        let resolve_to = hold.payee_resolved.unwrap_or_default();
        let cat_resolved = hold.category_resolved.is_some();
        let resolve_category_to = hold.category_resolved.unwrap_or_default();
        self.pending_reviews.push(PendingReview {
            external_id: external_id.to_string(),
            source_monzo_id: source_monzo_id.to_string(),
            raw_merchant: raw_merchant.to_string(),
            amount,
            date: date.to_string(),
            resolve_to,
            create_new: false,
            suggestion: hold.payee_suggestion,
            confidence: hold.payee_confidence,
            category_suggestion: hold.category_suggestion,
            category_confidence: hold.category_confidence,
            resolve_category_to,
            category_resolved: cat_resolved,
            telegram_message_id: None,
        });
        true
    }
}

/// Per-dimension settle state for a held review, passed to
/// [`State::add_pending_classified`]. Named fields prevent a silent
/// transposition of the same-typed `Option<String>` values at the call site.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ClassifiedHold {
    /// `Some(name)` pre-settles the payee dimension to an existing payee.
    pub payee_resolved: Option<String>,
    /// Suggestion prompted for when the payee dimension is unsettled.
    pub payee_suggestion: Option<String>,
    /// "high" | "medium" | "low" for the payee suggestion (display only).
    pub payee_confidence: Option<String>,
    /// `Some(name)` pre-settles the category dimension (empty = uncategorised).
    pub category_resolved: Option<String>,
    /// Suggestion prompted for when the category dimension is unsettled.
    pub category_suggestion: Option<String>,
    /// "high" | "medium" | "low" for the category suggestion (display only).
    pub category_confidence: Option<String>,
}

/// A pending review the user has resolved, ready to import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedReview {
    pub external_id: String,
    pub raw_merchant: String,
    pub amount: i64,
    pub date: String,
    /// Final payee name to use.
    pub payee_name: String,
    /// True if the payee must be created in Actual first.
    pub create_new: bool,
    /// Final category name to use, or `None` for uncategorised.
    pub category_name: Option<String>,
    /// Monzo account the resolved txn came from; routes its import.
    pub source_monzo_id: String,
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
        s.monzo.accounts.push(AccountLink {
            monzo_id: "acc_1".into(),
            actual_id: "uuid-1".into(),
            ..AccountLink::default()
        });
        s.payee_cache
            .insert("TFL TRAVEL CH".into(), "Transport for London".into());
        s.pending_reviews.push(PendingReview {
            external_id: "tx_cafe_save".into(),
            source_monzo_id: String::new(),
            raw_merchant: "SQ *SOME CAFE".into(),
            amount: -450,
            date: "2026-05-30".into(),
            resolve_to: String::new(),
            create_new: false,
            suggestion: None,
            confidence: None,
            category_suggestion: None,
            category_confidence: None,
            resolve_category_to: String::new(),
            category_resolved: true,
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
            source_monzo_id: String::new(),
            raw_merchant: "TFL TRAVEL CH".into(),
            amount: -275,
            date: "2026-05-30".into(),
            resolve_to: "Transport for London".into(),
            create_new: false,
            suggestion: None,
            confidence: None,
            category_suggestion: None,
            category_confidence: None,
            resolve_category_to: String::new(),
            category_resolved: true,
            telegram_message_id: None,
        });
        // create new under explicit name
        s.pending_reviews.push(PendingReview {
            external_id: "tx_cafe".into(),
            source_monzo_id: String::new(),
            raw_merchant: "SQ *SOME CAFE".into(),
            amount: -450,
            date: "2026-05-30".into(),
            resolve_to: "Some Cafe".into(),
            create_new: true,
            suggestion: None,
            confidence: None,
            category_suggestion: None,
            category_confidence: None,
            resolve_category_to: String::new(),
            category_resolved: true,
            telegram_message_id: None,
        });
        // unresolved -> stays
        s.pending_reviews.push(PendingReview {
            external_id: "tx_amzn".into(),
            source_monzo_id: String::new(),
            raw_merchant: "AMZNMKTPLACE".into(),
            amount: -1999,
            date: "2026-05-31".into(),
            resolve_to: String::new(),
            create_new: false,
            suggestion: None,
            confidence: None,
            category_suggestion: None,
            category_confidence: None,
            resolve_category_to: String::new(),
            category_resolved: true,
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
        let added1 = s.add_pending("tx1", "AMZNMKTPLACE", -1999, "2026-05-31", "acc_1");
        let added2 = s.add_pending("tx1", "AMZNMKTPLACE", -1999, "2026-05-31", "acc_1");
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
            source_monzo_id: String::new(),
            raw_merchant: "TFL TRAVEL CH".into(),
            amount: -275,
            date: "2026-05-30".into(),
            resolve_to: String::new(),
            create_new: false,
            suggestion: Some("Transport for London".into()),
            confidence: Some("medium".into()),
            category_suggestion: None,
            category_confidence: None,
            resolve_category_to: String::new(),
            category_resolved: true,
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
            "",
        );
        assert!(added);
        let p = &s.pending_reviews[0];
        assert_eq!(p.suggestion.as_deref(), Some("Local Cafe"));
        assert_eq!(p.confidence.as_deref(), Some("low"));
        assert!(p.telegram_message_id.is_none());
    }

    #[test]
    fn monzo_state_debug_redacts_refresh_token() {
        let m = MonzoState {
            refresh_token: "SECRET_RT".into(),
            account_id: "acc_1".into(),
            accounts: Vec::new(),
        };
        let s = format!("{m:?}");
        assert!(
            !s.contains("SECRET_RT"),
            "refresh_token must be redacted: {s}"
        );
        assert!(s.contains("acc_1"), "account_id is fine to show");
    }

    #[test]
    fn legacy_account_id_migrates_to_unmapped_link() {
        let toml = r#"
            [monzo]
            refresh_token = "rt"
            account_id = "acc_legacy"
        "#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), toml).unwrap();
        let s = State::load(tmp.path()).unwrap();
        assert_eq!(s.monzo.account_id, "", "legacy field cleared");
        assert_eq!(s.monzo.accounts.len(), 1);
        assert_eq!(s.monzo.accounts[0].monzo_id, "acc_legacy");
        assert_eq!(
            s.monzo.accounts[0].actual_id, "",
            "unmapped until `aba accounts`"
        );
    }

    #[test]
    fn accounts_round_trip_and_no_double_migrate() {
        let mut s = State::default();
        s.monzo.accounts.push(AccountLink {
            monzo_id: "acc_1".into(),
            monzo_name: "Personal".into(),
            actual_id: "uuid-1".into(),
            actual_name: "Monzo Personal".into(),
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        s.save(&path).unwrap();
        let loaded = State::load(&path).unwrap();
        assert_eq!(loaded.monzo.accounts, s.monzo.accounts);
    }

    #[test]
    fn add_pending_stores_source_monzo_id() {
        let mut s = State::default();
        assert!(s.add_pending_full("ext1", "Cafe", -450, "2026-05-30", None, None, "acc_2"));
        assert_eq!(s.pending_reviews[0].source_monzo_id, "acc_2");
    }

    #[test]
    fn take_resolved_waits_for_both_dimensions() {
        let mut s = State::default();
        // payee resolved, category NOT resolved -> must stay pending
        s.pending_reviews.push(PendingReview {
            external_id: "tx1".into(),
            source_monzo_id: "acc_1".into(),
            raw_merchant: "TESCO".into(),
            amount: -500,
            date: "2026-06-01".into(),
            resolve_to: "Tesco".into(),
            create_new: false,
            suggestion: None,
            confidence: None,
            category_suggestion: Some("Groceries".into()),
            category_confidence: Some("medium".into()),
            resolve_category_to: String::new(),
            category_resolved: false,
            telegram_message_id: None,
        });
        let taken = s.take_resolved();
        assert!(
            taken.is_empty(),
            "payee-only resolution must not import yet"
        );
        assert_eq!(s.pending_reviews.len(), 1);

        // now settle the category dimension via confirm
        assert!(s.confirm_category("tx1"));
        let taken2 = s.take_resolved();
        assert_eq!(taken2.len(), 1);
        assert_eq!(taken2[0].payee_name, "Tesco");
        assert_eq!(taken2[0].category_name.as_deref(), Some("Groceries"));
        // category folded into the category cache
        assert_eq!(
            s.category_cache.get("TESCO").map(String::as_str),
            Some("Groceries")
        );
    }

    #[test]
    fn skip_category_settles_as_uncategorised() {
        let mut s = State::default();
        s.pending_reviews.push(PendingReview {
            external_id: "tx2".into(),
            source_monzo_id: "acc_1".into(),
            raw_merchant: "ODD SHOP".into(),
            amount: -100,
            date: "2026-06-01".into(),
            resolve_to: "Odd Shop".into(),
            create_new: false,
            suggestion: None,
            confidence: None,
            category_suggestion: Some("Groceries".into()),
            category_confidence: Some("low".into()),
            resolve_category_to: String::new(),
            category_resolved: false,
            telegram_message_id: None,
        });
        assert!(s.skip_category("tx2"));
        let taken = s.take_resolved();
        assert_eq!(taken.len(), 1);
        assert_eq!(
            taken[0].category_name, None,
            "skipped category imports uncategorised"
        );
        // a skipped (empty) category is NOT written to the cache
        assert!(!s.category_cache.contains_key("ODD SHOP"));
    }

    #[test]
    fn remap_category_sets_resolve_target() {
        let mut s = State::default();
        s.pending_reviews.push(PendingReview {
            external_id: "tx3".into(),
            source_monzo_id: "acc_1".into(),
            raw_merchant: "X".into(),
            amount: -10,
            date: "2026-06-01".into(),
            resolve_to: "X Payee".into(),
            create_new: false,
            suggestion: None,
            confidence: None,
            category_suggestion: Some("Groceries".into()),
            category_confidence: Some("low".into()),
            resolve_category_to: String::new(),
            category_resolved: false,
            telegram_message_id: None,
        });
        assert!(s.remap_category("tx3", "Dining Out"));
        assert!(s.pending_reviews[0].category_resolved);
        assert_eq!(s.pending_reviews[0].resolve_category_to, "Dining Out");
        // end-to-end: both dimensions settled -> imports with the remapped category
        let taken = s.take_resolved();
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].category_name.as_deref(), Some("Dining Out"));
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
            "",
        );
        s.add_pending_full("b", "BBB", -2, "2026-06-01", None, None, "");
        s.add_pending_full(
            "c",
            "CCC",
            -3,
            "2026-06-01",
            Some("Costa".into()),
            Some("low".into()),
            "",
        );
        s.add_pending_full("d", "DDD", -4, "2026-06-01", None, None, "");
        // dedicated no-suggestion entry: confirm must refuse to set resolve_to.
        s.add_pending_full("e", "EEE", -5, "2026-06-01", None, None, "");

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
