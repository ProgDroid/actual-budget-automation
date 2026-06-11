use std::collections::{BTreeMap, BTreeSet};

use tracing::warn;

use crate::actual::shim_protocol::{Category, Payee};
use crate::banks::RawTransaction;
use crate::claude::{Classification, Confidence};
use crate::ids::ExternalTxId;

/// Result of splitting fetched transactions against the payee + category caches.
#[derive(Debug, PartialEq, Eq)]
pub struct ResolutionPlan {
    /// (`external_id`, `payee_name`, `category_name`) for FULL cache hits.
    /// Convention: an empty `category_name` String is the "uncategorised"
    /// sentinel (so `cached[i].2 == ""` means cached as uncategorised).
    pub cached: Vec<(ExternalTxId, String, String)>,
    /// Distinct raw merchant names that need a Claude lookup.
    pub needs_claude: Vec<String>,
}

pub fn plan_resolution(
    txs: &[RawTransaction],
    payee_cache: &BTreeMap<String, String>,
    category_cache: &BTreeMap<String, String>,
) -> ResolutionPlan {
    let mut cached = Vec::new();
    let mut needs_claude = Vec::new();
    for tx in txs {
        match (
            payee_cache.get(&tx.raw_merchant),
            category_cache.get(&tx.raw_merchant),
        ) {
            (Some(payee), Some(category)) => {
                cached.push((tx.external_id.clone(), payee.clone(), category.clone()));
            }
            // Payee cached but category not (legacy entry) -> needs Claude.
            _ if !needs_claude.contains(&tx.raw_merchant) => {
                needs_claude.push(tx.raw_merchant.clone());
            }
            _ => {}
        }
    }
    ResolutionPlan {
        cached,
        needs_claude,
    }
}

/// One held transaction with per-dimension state for review.
#[derive(Debug, PartialEq, Eq)]
pub struct HeldClassification {
    pub tx: RawTransaction,
    /// `Some(name)` if the payee was confident & exists (pre-settled, no prompt).
    pub payee_resolved: Option<String>,
    /// Suggestion to prompt for when the payee is uncertain.
    pub payee_suggestion: Option<String>,
    pub payee_confidence: Option<Confidence>,
    /// `Some(name)` if the category was confident & exists (pre-settled, no prompt).
    pub category_resolved: Option<String>,
    /// Suggestion to prompt for when the category is uncertain.
    pub category_suggestion: Option<String>,
    pub category_confidence: Option<Confidence>,
}

/// Result of applying Claude classifications, split by the auto-import cutoff.
#[derive(Debug, PartialEq, Eq)]
pub struct MatchOutcome {
    /// (`external_id`, `payee_name`, `category_name`) confident in BOTH dimensions.
    /// Convention: an empty `category_name` String is the "uncategorised"
    /// sentinel (so `matched[i].2 == ""` when no categories exist). Imported now.
    pub matched: Vec<(ExternalTxId, String, String)>,
    /// Held: at least one dimension uncertain.
    pub held: Vec<HeldClassification>,
}

/// Per-dimension settle state for the category dimension during gating.
enum CatState {
    Settled(String),
    Uncertain,
}

// `matches` (input) and `matched` (auto-imported output) are distinct concepts
// despite their lexical similarity.
#[allow(clippy::similar_names)]
pub fn apply_matches(
    txs: &[RawTransaction],
    matches: &[Classification],
    autoimport: Confidence,
    existing_payees: &BTreeSet<&str>,
    existing_categories: &BTreeSet<&str>,
    categories_available: bool,
) -> MatchOutcome {
    let lookup: BTreeMap<&str, &Classification> =
        matches.iter().map(|m| (m.raw.as_str(), m)).collect();
    let mut matched = Vec::new();
    let mut held = Vec::new();
    for tx in txs {
        let Some(m) = lookup.get(tx.raw_merchant.as_str()) else {
            // No classification at all -> hold, both dimensions unsettled
            // (category auto-settled to uncategorised if none exist).
            held.push(HeldClassification {
                tx: tx.clone(),
                payee_resolved: None,
                payee_suggestion: None,
                payee_confidence: None,
                category_resolved: if categories_available {
                    None
                } else {
                    Some(String::new())
                },
                category_suggestion: None,
                category_confidence: None,
            });
            continue;
        };

        // Payee dimension.
        let payee_ok = m.payee.as_deref().is_some_and(|p| {
            m.payee_confidence.rank() >= autoimport.rank() && existing_payees.contains(p)
        });

        // Category dimension. Not applicable (no categories) counts as
        // settled-uncategorised.
        let category_state: CatState = if !categories_available {
            CatState::Settled(String::new())
        } else if let Some(cat) = m.category.as_deref() {
            if m.category_confidence.rank() >= autoimport.rank()
                && existing_categories.contains(cat)
            {
                CatState::Settled(cat.to_string())
            } else {
                CatState::Uncertain
            }
        } else {
            CatState::Uncertain
        };

        if let (true, CatState::Settled(cat)) = (payee_ok, &category_state) {
            // Both confident -> import now.
            matched.push((
                tx.external_id.clone(),
                m.payee.clone().unwrap(),
                cat.clone(),
            ));
        } else {
            // Hold; pre-settle whichever dimension is confident.
            let payee_resolved = payee_ok.then(|| m.payee.clone().unwrap());
            let (payee_suggestion, payee_confidence) = if payee_ok {
                (None, None)
            } else {
                (
                    m.payee.clone(),
                    m.payee.as_ref().map(|_| m.payee_confidence),
                )
            };
            let (category_resolved, category_suggestion, category_confidence) =
                match &category_state {
                    CatState::Settled(cat) => (Some(cat.clone()), None, None),
                    CatState::Uncertain => (
                        None,
                        m.category.clone(),
                        m.category.as_ref().map(|_| m.category_confidence),
                    ),
                };
            held.push(HeldClassification {
                tx: tx.clone(),
                payee_resolved,
                payee_suggestion,
                payee_confidence,
                category_resolved,
                category_suggestion,
                category_confidence,
            });
        }
    }
    MatchOutcome { matched, held }
}

/// Map a category name to its id. Returns `None` (→ caller holds, fail-loud) if
/// the name is unknown OR ambiguous: Actual permits same-named categories in
/// different groups, and we must not silently pick the wrong one.
pub fn category_id_for<'a>(cats: &'a [Category], name: &str) -> Option<&'a str> {
    let mut matches = cats.iter().filter(|c| c.name == name);
    let first = matches.next()?;
    if matches.next().is_some() {
        warn!("category name '{name}' is ambiguous (>1 group); holding for review");
        return None;
    }
    Some(first.id.as_str())
}

/// Map a payee name to its id, for building no-auto-create import rows.
pub fn payee_id_for<'a>(payees: &'a [Payee], name: &str) -> Option<&'a str> {
    payees
        .iter()
        .find(|p| p.name == name)
        .map(|p| p.id.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::testutil::*;

    #[test]
    fn plan_needs_claude_unless_both_caches_hit() {
        let mut payee = BTreeMap::new();
        let mut cat = BTreeMap::new();
        payee.insert(
            "TFL TRAVEL CH".to_string(),
            "Transport for London".to_string(),
        );
        cat.insert("TFL TRAVEL CH".to_string(), "Transport".to_string());
        // AMZN only in payee cache -> still needs claude (no category)
        payee.insert("AMZNMKTPLACE".to_string(), "Amazon".to_string());
        let txs = vec![
            tx("1", "TFL TRAVEL CH", -275),
            tx("2", "AMZNMKTPLACE", -1999),
        ];
        let plan = plan_resolution(&txs, &payee, &cat);
        assert_eq!(
            plan.cached,
            vec![(
                "1".into(),
                "Transport for London".to_string(),
                "Transport".to_string()
            )]
        );
        assert_eq!(plan.needs_claude, vec!["AMZNMKTPLACE".to_string()]);
    }

    #[test]
    fn apply_matches_gates_on_both_dimensions() {
        let txs = vec![
            tx("1", "TESCO", -500),   // both high + exist -> matched
            tx("2", "CAFE", -450),    // payee high+exist, category low -> held, payee pre-settled
            tx("3", "MYSTERY", -100), // payee none -> held, payee unsettled
        ];
        let matches = vec![
            cls(
                "TESCO",
                Some("Tesco"),
                Confidence::High,
                Some("Groceries"),
                Confidence::High,
            ),
            cls(
                "CAFE",
                Some("Local Cafe"),
                Confidence::High,
                Some("Dining"),
                Confidence::Low,
            ),
            cls("MYSTERY", None, Confidence::Low, None, Confidence::Low),
        ];
        let payees: BTreeSet<&str> = ["Tesco", "Local Cafe"].into_iter().collect();
        let cats: BTreeSet<&str> = ["Groceries", "Dining"].into_iter().collect();
        let outcome = apply_matches(&txs, &matches, Confidence::High, &payees, &cats, true);

        assert_eq!(
            outcome.matched,
            vec![(
                ExternalTxId::from("1"),
                "Tesco".to_string(),
                "Groceries".to_string()
            )]
        );
        assert_eq!(outcome.held.len(), 2);

        let h2 = outcome
            .held
            .iter()
            .find(|h| h.tx.external_id.as_str() == "2")
            .unwrap();
        assert_eq!(h2.payee_resolved.as_deref(), Some("Local Cafe")); // pre-settled
        assert_eq!(h2.category_suggestion.as_deref(), Some("Dining"));
        assert_eq!(h2.category_resolved, None); // uncertain -> prompt

        let h3 = outcome
            .held
            .iter()
            .find(|h| h.tx.external_id.as_str() == "3")
            .unwrap();
        assert_eq!(h3.payee_resolved, None);
        assert_eq!(h3.payee_suggestion, None);
    }

    #[test]
    fn apply_matches_autosettles_category_when_none_exist() {
        let txs = vec![tx("1", "TESCO", -500)];
        let matches = vec![cls(
            "TESCO",
            Some("Tesco"),
            Confidence::High,
            None,
            Confidence::Low,
        )];
        let payees: BTreeSet<&str> = BTreeSet::from(["Tesco"]);
        let cats: BTreeSet<&str> = BTreeSet::new();
        // categories_available = false -> category dimension not applicable -> matched, category None
        let outcome = apply_matches(&txs, &matches, Confidence::High, &payees, &cats, false);
        assert_eq!(outcome.matched.len(), 1);
        assert_eq!(
            outcome.matched[0].2,
            String::new(),
            "uncategorised sentinel"
        );
    }

    #[test]
    fn category_id_for_holds_on_ambiguous_name() {
        use crate::actual::shim_protocol::Category;
        let cats = vec![
            Category {
                id: "c1".into(),
                name: "Misc".into(),
            },
            Category {
                id: "c2".into(),
                name: "Misc".into(),
            },
            Category {
                id: "c3".into(),
                name: "Groceries".into(),
            },
        ];
        assert_eq!(category_id_for(&cats, "Groceries"), Some("c3"));
        assert_eq!(
            category_id_for(&cats, "Misc"),
            None,
            "duplicate -> None (hold)"
        );
        assert_eq!(category_id_for(&cats, "Nope"), None);
    }
}
