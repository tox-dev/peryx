//! Give statically configured indexes a persisted repository identity at startup.

use std::time::{SystemTime, UNIX_EPOCH};

use peryx_identity::UserId;
use peryx_storage::meta::{DesiredRepository, MetaStore};

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

/// Give every configured index a persisted repository record.
///
/// Each index reconciles by route: a new route mints a record, an existing route reuses its id, so a
/// restart adds nothing and a later rename never re-homes a reference. Unchanged configuration bumps
/// no version. A route the store cannot hold as a repository, an over-long one for example, leaves
/// the batch unwritten and logs rather than failing an otherwise healthy boot.
pub fn reconcile_configured_repositories(meta: &MetaStore, configs: &[IndexConfig]) {
    let desired: Vec<DesiredRepository> = configs.iter().map(desired).collect();
    if let Err(error) = meta.reconcile_repositories(&desired, &system_actor(), unix_now()) {
        tracing::warn!(%error, "could not assign stable ids to configured repositories");
    }
}

#[cfg(test)]
#[path = "../../tests/unit/config/repository_migration/tests.rs"]
mod tests;
