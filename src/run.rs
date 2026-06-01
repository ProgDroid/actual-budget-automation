//! Run pipeline and auth bootstrap.

use std::collections::BTreeMap;

use crate::banks::RawTransaction;
use crate::claude::PayeeMatch;

/// Result of splitting fetched transactions against the payee cache.
#[derive(Debug, PartialEq)]
pub struct ResolutionPlan {
    /// (external_id, payee_name) for cache hits.
    pub cached: Vec<(String, String)>,
    /// Distinct raw merchant names that need a Claude lookup.
    pub needs_claude: Vec<String>,
}

pub fn plan_resolution(txs: &[RawTransaction], cache: &BTreeMap<String, String>) -> ResolutionPlan {
    let mut cached = Vec::new();
    let mut needs_claude = Vec::new();
    for tx in txs {
        if let Some(payee) = cache.get(&tx.raw_merchant) {
            cached.push((tx.external_id.clone(), payee.clone()));
        } else if !needs_claude.contains(&tx.raw_merchant) {
            needs_claude.push(tx.raw_merchant.clone());
        }
    }
    ResolutionPlan {
        cached,
        needs_claude,
    }
}

/// Result of applying Claude matches to the transactions that needed lookup.
#[derive(Debug, PartialEq)]
pub struct MatchOutcome {
    /// (external_id, payee_name) successfully matched.
    pub matched: Vec<(String, String)>,
    /// Transactions with no match — to be held for review.
    pub unmatched: Vec<RawTransaction>,
}

pub fn apply_matches(txs: &[RawTransaction], matches: &[PayeeMatch]) -> MatchOutcome {
    let lookup: BTreeMap<&str, Option<&String>> = matches
        .iter()
        .map(|m| (m.raw.as_str(), m.payee.as_ref()))
        .collect();
    let mut matched = Vec::new();
    let mut unmatched = Vec::new();
    for tx in txs {
        match lookup.get(tx.raw_merchant.as_str()) {
            Some(Some(payee)) => matched.push((tx.external_id.clone(), (*payee).clone())),
            _ => unmatched.push(tx.clone()),
        }
    }
    MatchOutcome { matched, unmatched }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::banks::RawTransaction;
    use crate::claude::PayeeMatch;
    use chrono::NaiveDate;
    use std::collections::BTreeMap;

    fn tx(id: &str, merchant: &str, amount: i64) -> RawTransaction {
        RawTransaction {
            external_id: id.into(),
            date: NaiveDate::from_ymd_opt(2026, 5, 30).unwrap(),
            amount_minor: amount,
            raw_merchant: merchant.into(),
        }
    }

    #[test]
    fn splits_cache_hits_unknowns_for_claude() {
        let mut cache = BTreeMap::new();
        cache.insert(
            "TFL TRAVEL CH".to_string(),
            "Transport for London".to_string(),
        );
        let txs = vec![
            tx("1", "TFL TRAVEL CH", -275),
            tx("2", "AMZNMKTPLACE", -1999),
        ];

        let plan = plan_resolution(&txs, &cache);
        assert_eq!(
            plan.cached,
            vec![("1".to_string(), "Transport for London".to_string())]
        );
        assert_eq!(plan.needs_claude, vec!["AMZNMKTPLACE".to_string()]);
    }

    #[test]
    fn applies_claude_matches_and_flags_unmatched() {
        let txs = vec![tx("2", "AMZNMKTPLACE", -1999), tx("3", "WEIRD XYZ", -100)];
        let matches = vec![
            PayeeMatch {
                raw: "AMZNMKTPLACE".into(),
                payee: Some("Amazon".into()),
            },
            PayeeMatch {
                raw: "WEIRD XYZ".into(),
                payee: None,
            },
        ];
        let outcome = apply_matches(&txs, &matches);
        assert_eq!(
            outcome.matched,
            vec![("2".to_string(), "Amazon".to_string())]
        );
        assert_eq!(outcome.unmatched.len(), 1);
        assert_eq!(outcome.unmatched[0].external_id, "3");
    }
}
