//! JSON protocol shared with the Node shim.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ShimRequest {
    GetAccounts,
    GetPayees,
    GetScheduled,
    GetCategories,
    ImportTransactions {
        account_id: String,
        transactions: Vec<ImportTxn>,
    },
    CreatePayee {
        name: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportTxn {
    pub date: String,
    pub amount: i64,
    /// Existing Actual payee **id** (never a name — avoids auto-create).
    pub payee: String,
    /// Existing Actual category **id**; omitted from the wire when `None`
    /// (Actual leaves the transaction uncategorised).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    pub imported_id: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Account {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Payee {
    pub id: String,
    pub name: String,
    /// Set (an account id) when this payee is a *transfer payee* — importing a
    /// transaction with this payee creates a linked transfer into that account.
    /// `None`/absent for ordinary payees.
    #[serde(default)]
    pub transfer_acct: Option<String>,
}

/// An assignable Actual category (real categories carry a `group_id`; the shim
/// filters out category *groups*).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Category {
    pub id: String,
    pub name: String,
}

/// Raw schedule row from the shim (`_payee` is an id resolved to a name later).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ScheduleRow {
    pub date: Option<String>,
    #[serde(default)]
    pub amount: Option<i64>,
    pub payee_id: Option<String>,
}

/// Returns `None`; used as a serde `default` for `Option<T>` fields where `T`
/// may not implement `Default`.
const fn none<T>() -> Option<T> {
    None
}

/// Flat shim envelope. The shim always emits `ok`; `data` xor `error` is
/// populated. An explicit discriminant (not `#[serde(untagged)]`) so a future
/// shim change can never silently misparse into the wrong variant.
#[derive(Debug, Deserialize)]
pub struct ShimResponse<T> {
    pub ok: bool,
    #[serde(default = "none")]
    pub data: Option<T>,
    #[serde(default)]
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_accounts_request_serialises_and_account_deserialises() {
        let req = ShimRequest::GetAccounts;
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"op":"get_accounts"}"#);

        let acct: Account =
            serde_json::from_str(r#"{"id":"uuid-1","name":"Monzo Personal"}"#).unwrap();
        assert_eq!(
            acct,
            Account {
                id: "uuid-1".into(),
                name: "Monzo Personal".into()
            }
        );
    }

    #[test]
    fn import_request_serialises_with_op_tag() {
        let req = ShimRequest::ImportTransactions {
            account_id: "acc_1".into(),
            transactions: vec![ImportTxn {
                date: "2026-05-30".into(),
                amount: -450,
                payee: "payee_id_1".into(),
                category: None,
                imported_id: "tx_1".into(),
            }],
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["op"], "import_transactions");
        assert_eq!(v["transactions"][0]["payee"], "payee_id_1");
    }

    #[test]
    fn get_categories_request_serialises_and_category_deserialises() {
        let req = ShimRequest::GetCategories;
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            r#"{"op":"get_categories"}"#
        );

        let cat: Category = serde_json::from_str(r#"{"id":"cat-1","name":"Groceries"}"#).unwrap();
        assert_eq!(
            cat,
            Category {
                id: "cat-1".into(),
                name: "Groceries".into()
            }
        );
    }

    #[test]
    fn import_txn_omits_category_when_none_and_includes_when_some() {
        let none = ImportTxn {
            date: "2026-05-30".into(),
            amount: -450,
            payee: "p1".into(),
            category: None,
            imported_id: "tx1".into(),
        };
        let v = serde_json::to_value(&none).unwrap();
        assert!(
            v.get("category").is_none(),
            "category must be omitted when None"
        );

        let some = ImportTxn {
            date: "2026-05-30".into(),
            amount: -450,
            payee: "p1".into(),
            category: Some("cat-1".into()),
            imported_id: "tx1".into(),
        };
        let v2 = serde_json::to_value(&some).unwrap();
        assert_eq!(v2["category"], "cat-1");
    }

    #[test]
    fn schedule_row_accepts_null_date() {
        let row: ScheduleRow = serde_json::from_value(serde_json::json!({
            "date": null, "amount": 0, "payee_id": null
        }))
        .unwrap();
        assert_eq!(row.date, None);
        assert_eq!(row.amount, Some(0));
    }

    #[test]
    fn schedule_row_accepts_null_amount() {
        let row: ScheduleRow = serde_json::from_value(serde_json::json!({
            "date": "2026-06-01", "amount": null, "payee_id": "p1"
        }))
        .unwrap();
        assert_eq!(row.amount, None);
    }

    #[test]
    fn schedule_row_numeric_amount_is_some() {
        let row: ScheduleRow = serde_json::from_value(serde_json::json!({
            "date": "2026-06-01", "amount": -1500, "payee_id": "p1"
        }))
        .unwrap();
        assert_eq!(row.amount, Some(-1500));
    }

    #[test]
    fn response_parses_ok_and_error_via_discriminant() {
        let ok: ShimResponse<Vec<Payee>> = serde_json::from_value(serde_json::json!({
            "ok": true, "data": [{ "id": "p1", "name": "Amazon" }]
        }))
        .unwrap();
        assert!(ok.ok);
        assert_eq!(ok.data.unwrap().len(), 1);
        assert_eq!(ok.error, None);

        let err: ShimResponse<Vec<Payee>> = serde_json::from_value(serde_json::json!({
            "ok": false, "error": "boom"
        }))
        .unwrap();
        assert!(!err.ok);
        assert!(err.data.is_none());
        assert_eq!(err.error.as_deref(), Some("boom"));
    }

    #[test]
    fn response_ok_true_without_data_is_parseable_but_dataless() {
        // The mod.rs caller turns this into an explicit error instead of a panic.
        let r: ShimResponse<Vec<Payee>> =
            serde_json::from_value(serde_json::json!({ "ok": true })).unwrap();
        assert!(r.ok);
        assert!(r.data.is_none());
    }
}
