//! Scheduled-transaction skip and payee normalisation.

/// Lowercase and keep only alphanumerics — a stable key for fuzzy comparison.
pub fn normalise(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Similarity in [0,1] between two payee-ish strings, comparing normalised forms.
pub fn payee_similarity(a: &str, b: &str) -> f64 {
    strsim::jaro_winkler(&normalise(a), &normalise(b))
}

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

use chrono::NaiveDate;

use crate::banks::RawTransaction;

/// A scheduled transaction's next occurrence, as returned by the Actual shim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    /// Fixed amount in minor units, or `None` for variable/ranged schedules.
    pub amount: Option<i64>,
    pub date: NaiveDate,
    pub payee: String,
}

/// True if `tx` looks like a manifestation of any schedule.
///
/// For fixed-amount schedules (`amount` is `Some`): requires exact amount,
/// date within `date_tolerance_days`, and payee similarity >= `payee_threshold`.
/// For variable/ranged schedules (`amount` is `None`): matches on date + payee only,
/// ignoring the transaction amount.
pub fn matches_scheduled(
    tx: &RawTransaction,
    schedules: &[Schedule],
    date_tolerance_days: i64,
    payee_threshold: f64,
) -> bool {
    schedules.iter().any(|s| {
        let date_ok = (s.date - tx.date).num_days().abs() <= date_tolerance_days;
        let payee_ok = payee_similarity(&s.payee, &tx.raw_merchant) >= payee_threshold;
        // variable/ranged schedule (None) ignores amount
        let amount_ok = s.amount.is_none_or(|a| a == tx.amount_minor);
        amount_ok && date_ok && payee_ok
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
            source_monzo_id: "acc_1".into(),
        }
    }

    #[test]
    fn matches_scheduled_within_tolerance() {
        let schedules = vec![Schedule {
            amount: Some(-1500),
            date: d(2026, 6, 1),
            payee: "Netflix".into(),
        }];
        let t = tx(-1500, d(2026, 5, 30), "NETFLIX.COM");
        assert!(matches_scheduled(&t, &schedules, 3, 0.85));
    }

    #[test]
    fn no_match_when_amount_differs() {
        let schedules = vec![Schedule {
            amount: Some(-1500),
            date: d(2026, 6, 1),
            payee: "Netflix".into(),
        }];
        let t = tx(-1499, d(2026, 6, 1), "NETFLIX.COM");
        assert!(!matches_scheduled(&t, &schedules, 3, 0.85));
    }

    #[test]
    fn no_match_when_date_out_of_tolerance() {
        let schedules = vec![Schedule {
            amount: Some(-1500),
            date: d(2026, 6, 1),
            payee: "Netflix".into(),
        }];
        let t = tx(-1500, d(2026, 6, 10), "NETFLIX.COM");
        assert!(!matches_scheduled(&t, &schedules, 3, 0.85));
    }

    #[test]
    fn no_match_when_payee_unrelated() {
        let schedules = vec![Schedule {
            amount: Some(-1500),
            date: d(2026, 6, 1),
            payee: "Netflix".into(),
        }];
        let t = tx(-1500, d(2026, 6, 1), "AMAZON PRIME");
        assert!(!matches_scheduled(&t, &schedules, 3, 0.85));
    }

    #[test]
    fn variable_amount_matches_on_date_and_payee_regardless_of_amount() {
        let schedules = vec![Schedule {
            amount: None, // variable / ranged schedule
            date: d(2026, 6, 1),
            payee: "Netflix".into(),
        }];
        // Any amount near the date with a matching payee should be treated as the
        // schedule's manifestation and skipped.
        let t = tx(-9999, d(2026, 5, 30), "NETFLIX.COM");
        assert!(matches_scheduled(&t, &schedules, 3, 0.85));
    }

    #[test]
    fn variable_amount_still_requires_date_and_payee() {
        let schedules = vec![Schedule {
            amount: None,
            date: d(2026, 6, 1),
            payee: "Netflix".into(),
        }];
        // Wrong payee -> no match even though amount is ignored.
        let wrong_payee = tx(-9999, d(2026, 6, 1), "AMAZON PRIME");
        assert!(!matches_scheduled(&wrong_payee, &schedules, 3, 0.85));
        // Out-of-tolerance date -> no match.
        let wrong_date = tx(-9999, d(2026, 6, 10), "NETFLIX.COM");
        assert!(!matches_scheduled(&wrong_date, &schedules, 3, 0.85));
    }

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
}
