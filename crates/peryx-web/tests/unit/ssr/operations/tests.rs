use peryx_core::{OperationRow, OperationsHealth, OperationsView, UiOperationStatus};
use peryx_storage::meta::{OperationOutcomeHealth, OperationOutcomePage, OperationOutcomeRow, OperationState};

use super::operations_for_class;

#[test]
fn operations_projects_operator_health_without_rows() {
    assert_eq!(
        operations_for_class(
            40,
            Ok(OperationOutcomeHealth {
                pending: 1,
                published: 2,
                failed: 3,
                expired: 4,
            }),
            None,
        ),
        Ok(OperationsView {
            captured_at: 40,
            health: OperationsHealth {
                pending: 1,
                published: 2,
                failed: 3,
                expired: 4,
                total: 10,
            },
            rows: None,
            next_cursor: None,
        })
    );
}

#[test]
fn operations_projects_administrator_rows_and_cursor() {
    assert_eq!(
        operations_for_class(
            10,
            Ok(OperationOutcomeHealth::default()),
            Some(Ok(OperationOutcomePage {
                rows: vec![
                    OperationOutcomeRow {
                        operation: "failed".to_owned(),
                        state: OperationState::Failed,
                        expiry_unix: Some(1),
                        updated_at_unix: 2,
                    },
                    OperationOutcomeRow {
                        operation: "pending".to_owned(),
                        state: OperationState::Pending,
                        expiry_unix: None,
                        updated_at_unix: 3,
                    },
                    OperationOutcomeRow {
                        operation: "pending-until".to_owned(),
                        state: OperationState::Pending,
                        expiry_unix: Some(11),
                        updated_at_unix: 4,
                    },
                    OperationOutcomeRow {
                        operation: "published".to_owned(),
                        state: OperationState::Published,
                        expiry_unix: Some(1),
                        updated_at_unix: 5,
                    },
                    OperationOutcomeRow {
                        operation: "expired".to_owned(),
                        state: OperationState::Pending,
                        expiry_unix: Some(10),
                        updated_at_unix: 6,
                    },
                ],
                next_cursor: Some("published".to_owned()),
            })),
        ),
        Ok(OperationsView {
            captured_at: 10,
            health: OperationsHealth::default(),
            rows: Some(vec![
                OperationRow {
                    operation: "failed".to_owned(),
                    status: UiOperationStatus::Failed,
                    updated_at: 2,
                    expires_at: Some(1),
                },
                OperationRow {
                    operation: "pending".to_owned(),
                    status: UiOperationStatus::Pending,
                    updated_at: 3,
                    expires_at: None,
                },
                OperationRow {
                    operation: "pending-until".to_owned(),
                    status: UiOperationStatus::Pending,
                    updated_at: 4,
                    expires_at: Some(11),
                },
                OperationRow {
                    operation: "published".to_owned(),
                    status: UiOperationStatus::Published,
                    updated_at: 5,
                    expires_at: Some(1),
                },
                OperationRow {
                    operation: "expired".to_owned(),
                    status: UiOperationStatus::Expired,
                    updated_at: 6,
                    expires_at: Some(10),
                },
            ]),
            next_cursor: Some("published".to_owned()),
        })
    );
}

#[test]
fn operations_reports_health_errors() {
    assert_eq!(
        operations_for_class(40, Err(()), None),
        Err("Operation health could not be read.".to_owned())
    );
}

#[test]
fn operations_reports_row_errors() {
    assert_eq!(
        operations_for_class(40, Ok(OperationOutcomeHealth::default()), Some(Err(())),),
        Err("Operation rows could not be read.".to_owned())
    );
}
