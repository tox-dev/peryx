use std::time::{SystemTime, UNIX_EPOCH};

use peryx_identity::UserId;
use peryx_storage::meta::{DesiredRepository, MetaStore, ReconcileRepositoryError};

use super::IndexConfig;

/// Provenance for a repository carried over from static configuration. The reconcile runs before any
/// operator can act, so a reserved principal marks the origin until an operator edits the record.
fn system_actor() -> UserId {
    serde_json::from_value(serde_json::Value::String("usr_system".to_owned()))
        .expect("a transparent UserId deserializes from any string")
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs().try_into().unwrap_or(i64::MAX))
}

fn desired(config: &IndexConfig) -> DesiredRepository {
    DesiredRepository {
        route: config.route.clone(),
        display_name: config.name.clone(),
        ecosystem: config.ecosystem.as_str().to_owned(),
        definition: serde_json::json!({}),
    }
}

/// Persist repository identities before the server exposes configured routes.
///
/// Route matching keeps IDs stable and advances versions only when a stored definition changes.
///
/// # Errors
/// Returns an error when validation or persistence prevents the atomic batch.
pub fn reconcile_configured_repositories(
    meta: &MetaStore,
    configs: &[IndexConfig],
) -> Result<(), ReconcileRepositoryError> {
    let desired: Vec<DesiredRepository> = configs.iter().map(desired).collect();
    meta.reconcile_repositories(&desired, &system_actor(), unix_now())
        .map(drop)
}

#[cfg(test)]
#[path = "../../tests/unit/config/repository_migration/tests.rs"]
mod tests;
