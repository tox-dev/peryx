use std::sync::Arc;

use futures_util::StreamExt as _;
use leptos::prelude::*;
use leptos_router::location::RequestUrl;
use peryx_core::{OperationRow, OperationsHealth, OperationsView, UiOperationStatus};
use peryx_driver::AppState;
use peryx_storage::blob::BlobStorage;
use peryx_storage::meta::MetaStore;

use super::{OperationsBody, PendingOperations};

#[tokio::test(flavor = "current_thread")]
async fn pending_operations_reports_public_access_denial() {
    match any_spawner::Executor::init_tokio() {
        Ok(()) | Err(any_spawner::ExecutorError::AlreadySet) => {}
    }
    let directory = tempfile::tempdir().unwrap();
    let owner = Owner::new();
    owner.set();
    provide_context(Arc::new(AppState::new(
        MetaStore::open(directory.path().join("peryx.redb")).unwrap(),
        BlobStorage::filesystem(directory.path().join("blobs")),
        60,
        Vec::new(),
    )));
    provide_context(RequestUrl::new("/operations"));

    let html = view! { <PendingOperations /> }
        .to_html_stream_in_order()
        .collect::<String>()
        .await;

    assert!(html.contains("You do not have access to operation health."), "{html}");
}

#[test]
fn operations_body_renders_rows_and_pager() {
    let owner = Owner::new();
    owner.set();
    let (_, set_cursor) = signal(None::<String>);
    let html = view! {
        <OperationsBody
            view=OperationsView {
                captured_at: 0,
                health: OperationsHealth { pending: 1, published: 2, failed: 3, expired: 4, total: 10 },
                rows: Some(vec![
                    OperationRow {
                        operation: "op-published".to_owned(),
                        status: UiOperationStatus::Published,
                        updated_at: 0,
                        expires_at: Some(1),
                    },
                    OperationRow {
                        operation: "op-pending".to_owned(),
                        status: UiOperationStatus::Pending,
                        updated_at: 0,
                        expires_at: None,
                    },
                    OperationRow {
                        operation: "op-failed".to_owned(),
                        status: UiOperationStatus::Failed,
                        updated_at: 0,
                        expires_at: None,
                    },
                    OperationRow {
                        operation: "op-expired".to_owned(),
                        status: UiOperationStatus::Expired,
                        updated_at: 0,
                        expires_at: None,
                    },
                ]),
                next_cursor: Some("next".to_owned()),
            }
            set_cursor
        />
    }
    .to_html();
    for (operation, status) in [
        ("op-published", "Published"),
        ("op-pending", "Pending"),
        ("op-failed", "Failed"),
        ("op-expired", "Expired"),
    ] {
        let (_, rest) = html.split_once(operation).expect("operation row is rendered");
        let (row, _) = rest.split_once("</tr>").expect("operation row is complete");
        assert!(
            row.contains(&format!(">{status}</span>")),
            "missing status {status:?} in {row}"
        );
    }
    assert!(html.contains("Next page"), "{html}");
    assert!(html.contains("1970-01-01T00:00:01Z"), "{html}");
    let withheld = view! {
        <OperationsBody
            view=OperationsView {
                captured_at: 0,
                health: OperationsHealth { pending: 1, published: 2, failed: 3, expired: 4, total: 10 },
                rows: None,
                next_cursor: None,
            }
            set_cursor
        />
    }
    .to_html();
    let empty = view! {
        <OperationsBody
            view=OperationsView {
                captured_at: 0,
                health: OperationsHealth { pending: 1, published: 2, failed: 3, expired: 4, total: 10 },
                rows: Some(Vec::new()),
                next_cursor: None,
            }
            set_cursor
        />
    }
    .to_html();
    assert!(withheld.contains("need administrator access"), "{withheld}");
    assert!(empty.contains("No operations are recorded yet."), "{empty}");
}
