//! JSON protocol shared with the Node shim.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ShimRequest {
    GetPayees,
    GetScheduled,
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
    pub imported_id: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Payee {
    pub id: String,
    pub name: String,
}

/// Raw schedule row from the shim (`_payee` is an id resolved to a name later).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ScheduleRow {
    pub date: String,
    pub amount: i64,
    pub payee_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ShimResponse<T> {
    Ok { ok: bool, data: T },
    Err { ok: bool, error: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_request_serialises_with_op_tag() {
        let req = ShimRequest::ImportTransactions {
            account_id: "acc_1".into(),
            transactions: vec![ImportTxn {
                date: "2026-05-30".into(),
                amount: -450,
                payee: "payee_id_1".into(),
                imported_id: "tx_1".into(),
            }],
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["op"], "import_transactions");
        assert_eq!(v["transactions"][0]["payee"], "payee_id_1");
    }

    #[test]
    fn response_deserialises_ok_and_error() {
        let ok: ShimResponse<Vec<Payee>> = serde_json::from_value(serde_json::json!({
            "ok": true, "data": [{ "id": "p1", "name": "Amazon" }]
        }))
        .unwrap();
        assert!(matches!(ok, ShimResponse::Ok { .. }));

        let err: ShimResponse<Vec<Payee>> = serde_json::from_value(serde_json::json!({
            "ok": false, "error": "boom"
        }))
        .unwrap();
        assert!(matches!(err, ShimResponse::Err { .. }));
    }
}
