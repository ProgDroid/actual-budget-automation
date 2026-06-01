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
}
