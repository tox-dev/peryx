use std::collections::BTreeMap;

use peryx_storage::meta::{MetaStore, RepositoryQuery};

use super::reconcile_configured_repositories;
use crate::config::Config;

fn routes_to_id_and_version(store: &MetaStore) -> BTreeMap<String, (String, u64)> {
    store
        .list_repositories(&RepositoryQuery {
            limit: 100,
            ..RepositoryQuery::default()
        })
        .unwrap()
        .repositories
        .into_iter()
        .map(|record| (record.route, (record.id.as_str().to_owned(), record.version)))
        .collect()
}

#[test]
fn test_reconcile_assigns_stable_ids_idempotently_across_boots() {
    let config = Config::default();
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();

    reconcile_configured_repositories(&store, &config.indexes);
    let first_boot = routes_to_id_and_version(&store);

    assert_eq!(first_boot.len(), config.indexes.len());
    assert!(first_boot.values().all(|(id, version)| !id.is_empty() && *version == 1));

    reconcile_configured_repositories(&store, &config.indexes);

    assert_eq!(routes_to_id_and_version(&store), first_boot);
}

#[test]
fn test_reconcile_renames_default_virtual_indexes_without_changing_ids() {
    let mut old = Config::default();
    old.indexes
        .iter_mut()
        .find(|index| index.route == "root/pypi")
        .unwrap()
        .name = "root/pypi".to_owned();
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    reconcile_configured_repositories(&store, &old.indexes);
    let previous = store.repository_by_route("root/pypi").unwrap().unwrap();

    reconcile_configured_repositories(&store, &Config::default().indexes);
    let current = store.repository_by_route("root/pypi").unwrap().unwrap();

    assert_eq!(current.id, previous.id);
    assert_eq!(current.display_name, "root-pypi");
    assert_eq!(current.version, previous.version + 1);
}

#[test]
fn test_reconcile_writes_nothing_when_a_route_cannot_be_a_repository() {
    let mut config = Config::default();
    config.indexes[0].route = "r".repeat(513);
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();

    reconcile_configured_repositories(&store, &config.indexes);

    assert!(routes_to_id_and_version(&store).is_empty());
}
