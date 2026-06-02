//! Bank source trait and shared types.

pub mod monzo;

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};

use crate::state::State;

/// A transaction as fetched from a bank, before payee normalisation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawTransaction {
    /// Bank's stable id; used as Actual's `imported_id` for idempotency.
    pub external_id: String,
    pub date: NaiveDate,
    /// Integer minor units (pence). Negative = outflow.
    pub amount_minor: i64,
    /// Best raw merchant string (merchant name, fallback description).
    pub raw_merchant: String,
}

#[derive(Debug, thiserror::Error)]
pub enum BankError {
    /// Unrecoverable auth state: caller notifies and skips this bank.
    #[error("authentication expired: {0}")]
    AuthExpired(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// A pluggable bank source. Monzo this phase; `GoCardless` in Phase 2.
#[async_trait]
pub trait BankSource {
    fn name(&self) -> &'static str;

    /// Ensure a usable access token, refreshing if needed and persisting any
    /// rotated refresh token into `state`. Returns `AuthExpired` when a human
    /// re-auth is required.
    async fn ensure_auth(&mut self, state: &mut State) -> Result<(), BankError>;

    /// Fetch transactions strictly after `since`.
    async fn fetch_since(&self, since: DateTime<Utc>) -> Result<Vec<RawTransaction>, BankError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    #[test]
    fn raw_transaction_constructs() {
        let tx = RawTransaction {
            external_id: "tx_1".into(),
            date: NaiveDate::from_ymd_opt(2026, 5, 30).unwrap(),
            amount_minor: -450,
            raw_merchant: "SQ *SOME CAFE".into(),
        };
        assert_eq!(tx.amount_minor, -450);
        assert_eq!(tx.raw_merchant, "SQ *SOME CAFE");
    }

    #[test]
    fn auth_expired_displays() {
        let e = BankError::AuthExpired("token dead".into());
        assert!(e.to_string().contains("token dead"));
    }
}
