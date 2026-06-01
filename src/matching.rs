//! Scheduled-transaction skip and payee normalisation.

/// Lowercase and keep only alphanumerics — a stable key for fuzzy comparison.
pub fn normalise(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Similarity in [0,1] between two payee-ish strings, comparing normalised forms.
pub fn payee_similarity(a: &str, b: &str) -> f64 {
    strsim::jaro_winkler(&normalise(a), &normalise(b))
}

use chrono::NaiveDate;

use crate::banks::RawTransaction;

/// A scheduled transaction's next occurrence, as returned by the Actual shim.
#[derive(Debug, Clone, PartialEq)]
pub struct Schedule {
    pub amount: i64,
    pub date: NaiveDate,
    pub payee: String,
}

/// True if `tx` looks like a manifestation of any schedule: exact amount,
/// date within `date_tolerance_days`, and payee similarity >= `payee_threshold`.
pub fn matches_scheduled(
    tx: &RawTransaction,
    schedules: &[Schedule],
    date_tolerance_days: i64,
    payee_threshold: f64,
) -> bool {
    schedules.iter().any(|s| {
        s.amount == tx.amount_minor
            && (s.date - tx.date).num_days().abs() <= date_tolerance_days
            && payee_similarity(&s.payee, &tx.raw_merchant) >= payee_threshold
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_strips_case_and_punctuation() {
        assert_eq!(normalise("SQ *Some Cafe!"), "sqsomecafe");
        assert_eq!(normalise("TFL TRAVEL CH"), "tfltravelch");
    }

    #[test]
    fn similarity_high_for_close_names() {
        assert!(payee_similarity("Transport for London", "Transport for London") > 0.99);
        assert!(payee_similarity("TFL TRAVEL", "TFL TRAVEL CH") > 0.85);
        assert!(payee_similarity("Kroger", "Amazon") < 0.7);
    }

    use crate::banks::RawTransaction;
    use chrono::NaiveDate;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn tx(amount: i64, date: NaiveDate, merchant: &str) -> RawTransaction {
        RawTransaction {
            external_id: "x".into(),
            date,
            amount_minor: amount,
            raw_merchant: merchant.into(),
        }
    }

    #[test]
    fn matches_scheduled_within_tolerance() {
        let schedules = vec![Schedule {
            amount: -1500,
            date: d(2026, 6, 1),
            payee: "Netflix".into(),
        }];
        let t = tx(-1500, d(2026, 5, 30), "NETFLIX.COM");
        assert!(matches_scheduled(&t, &schedules, 3, 0.85));
    }

    #[test]
    fn no_match_when_amount_differs() {
        let schedules = vec![Schedule {
            amount: -1500,
            date: d(2026, 6, 1),
            payee: "Netflix".into(),
        }];
        let t = tx(-1499, d(2026, 6, 1), "NETFLIX.COM");
        assert!(!matches_scheduled(&t, &schedules, 3, 0.85));
    }

    #[test]
    fn no_match_when_date_out_of_tolerance() {
        let schedules = vec![Schedule {
            amount: -1500,
            date: d(2026, 6, 1),
            payee: "Netflix".into(),
        }];
        let t = tx(-1500, d(2026, 6, 10), "NETFLIX.COM");
        assert!(!matches_scheduled(&t, &schedules, 3, 0.85));
    }

    #[test]
    fn no_match_when_payee_unrelated() {
        let schedules = vec![Schedule {
            amount: -1500,
            date: d(2026, 6, 1),
            payee: "Netflix".into(),
        }];
        let t = tx(-1500, d(2026, 6, 1), "AMAZON PRIME");
        assert!(!matches_scheduled(&t, &schedules, 3, 0.85));
    }
}
