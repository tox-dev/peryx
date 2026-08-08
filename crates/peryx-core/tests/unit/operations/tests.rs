use super::{OperationRow, OperationsHealth, OperationsView};
use crate::view::UiOperationStatus;

#[test]
fn test_view_round_trips_through_json() {
    let view = OperationsView {
        captured_at: 1_800_000_000,
        health: OperationsHealth {
            pending: 2,
            published: 5,
            failed: 1,
            expired: 1,
            total: 9,
        },
        rows: Some(vec![OperationRow {
            operation: "op-1".to_owned(),
            status: UiOperationStatus::Pending,
            updated_at: 1_800_000_000,
            expires_at: Some(1_800_000_600),
        }]),
        next_cursor: Some("op-1".to_owned()),
    };
    let encoded = serde_json::to_string(&view).unwrap();
    assert_eq!(serde_json::from_str::<OperationsView>(&encoded).unwrap(), view);
}

#[test]
fn test_operator_view_omits_rows_and_cursor() {
    let view = OperationsView {
        captured_at: 7,
        health: OperationsHealth::default(),
        rows: None,
        next_cursor: None,
    };
    let value: serde_json::Value = serde_json::from_str(&serde_json::to_string(&view).unwrap()).unwrap();
    assert!(value.get("rows").is_none(), "{value}");
    assert!(value.get("next_cursor").is_none(), "{value}");
    assert_eq!(value["health"]["total"], 0);
}

#[test]
fn test_a_row_without_a_deadline_omits_the_expiry() {
    let row = OperationRow {
        operation: "op-2".to_owned(),
        status: UiOperationStatus::Published,
        updated_at: 42,
        expires_at: None,
    };
    let encoded = serde_json::to_string(&row).unwrap();
    assert!(
        !encoded.contains("expires_at"),
        "a deadline-free row omits expiry: {encoded}"
    );
    assert_eq!(serde_json::from_str::<OperationRow>(&encoded).unwrap(), row);
}
