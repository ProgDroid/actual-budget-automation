use std::collections::BTreeMap;

use chrono::NaiveDate;
use tracing::warn;

use crate::actual::shim_protocol::{Payee, ScheduleRow};
use crate::banks::RawTransaction;
use crate::matching::Schedule;
use crate::state::State;

/// Hold a transaction whose source Monzo account is not mapped to an Actual
/// account (no routing target yet). Payee/category default to unsettled.
pub fn hold_unmapped(state: &mut State, tx: &RawTransaction, held: &mut usize) {
    let date = tx.date.format("%Y-%m-%d").to_string();
    if state.add_pending(
        &tx.external_id,
        &tx.raw_merchant,
        tx.amount_minor,
        &date,
        &tx.source_monzo_id,
    ) {
        *held += 1;
    }
}

/// Build `Schedule`s for matching by resolving each row's payee id to a name.
pub fn schedules_from_rows(rows: Vec<ScheduleRow>, payees: &[Payee]) -> Vec<Schedule> {
    let by_id: BTreeMap<&str, &str> = payees
        .iter()
        .map(|p| (p.id.as_str(), p.name.as_str()))
        .collect();
    rows.into_iter()
        .filter_map(|r| {
            let Some(date_str) = r.date.as_deref() else {
                warn!("schedule dropped: missing date (payee_id={:?})", r.payee_id);
                return None;
            };
            let date = match NaiveDate::parse_from_str(date_str, "%Y-%m-%d") {
                Ok(d) => d,
                Err(e) => {
                    warn!("schedule dropped: unparseable date '{date_str}': {e}");
                    return None;
                }
            };
            let Some(payee) = r.payee_id.as_deref().and_then(|id| by_id.get(id).copied()) else {
                warn!(
                    "schedule dropped: payee_id {:?} not in payee list",
                    r.payee_id
                );
                return None;
            };
            Some(Schedule {
                amount: r.amount,
                date,
                payee: payee.to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::actual::shim_protocol::{Payee, ScheduleRow};

    #[test]
    fn schedules_from_rows_drops_bad_date_and_missing_payee_loudly() {
        let payees = vec![Payee {
            id: "p1".into(),
            name: "Rent Co".into(),
            transfer_acct: None,
        }];
        let rows = vec![
            // good
            ScheduleRow {
                date: Some("2026-06-01".into()),
                amount: Some(-1000),
                payee_id: Some("p1".into()),
            },
            // bad date -> dropped
            ScheduleRow {
                date: Some("not-a-date".into()),
                amount: Some(-1000),
                payee_id: Some("p1".into()),
            },
            // missing date -> dropped
            ScheduleRow {
                date: None,
                amount: Some(-1000),
                payee_id: Some("p1".into()),
            },
            // payee not in list -> dropped
            ScheduleRow {
                date: Some("2026-06-02".into()),
                amount: Some(-1000),
                payee_id: Some("nope".into()),
            },
        ];
        let schedules = schedules_from_rows(rows, &payees);
        assert_eq!(schedules.len(), 1, "only the fully-valid row survives");
        assert_eq!(schedules[0].payee, "Rent Co");
    }
}
