use peryx_ha::{
    ArtifactPlacement, ArtifactPlacementHealth, ArtifactPlacementPage, ArtifactPlacementQuery, ArtifactPlacementStore,
    ArtifactSource, ByteAvailability,
};

use crate::meta::MetaStore;

use super::distributed_store as store;

#[test]
fn test_local_reads_treat_absent_distributed_placement_state_as_empty() {
    let (_dir, store) = super::store();

    assert_eq!(store.get_artifact_placement("sha256:missing").unwrap(), None);
    assert_eq!(store.count_artifact_placements().unwrap(), 0);
    assert_eq!(
        store
            .list_artifact_placements(&ArtifactPlacementQuery::default())
            .unwrap(),
        ArtifactPlacementPage {
            rows: Vec::new(),
            next_cursor: None,
        }
    );
    assert_eq!(
        store.artifact_placement_health().unwrap(),
        ArtifactPlacementHealth::default()
    );
    let initial = ArtifactPlacement {
        source: ArtifactSource::Hosted,
        availability: ByteAvailability::Unavailable,
    };
    store.put_artifact_placement("sha256:missing", &initial).unwrap();
    assert_eq!(store.get_artifact_placement("sha256:missing").unwrap(), Some(initial));
    let replacement = ArtifactPlacement {
        source: ArtifactSource::Proxy,
        availability: ByteAvailability::RemoteOnly,
    };
    assert!(
        store
            .compare_and_put_artifact_placement("sha256:missing", &initial, &replacement)
            .unwrap()
    );
    assert_eq!(
        store.get_artifact_placement("sha256:missing").unwrap(),
        Some(replacement)
    );
    assert!(store.delete_artifact_placement("sha256:missing").unwrap());
    assert!(!store.delete_artifact_placement("sha256:missing").unwrap());
    assert_eq!(store.get_artifact_placement("sha256:missing").unwrap(), None);
    assert_eq!(store.count_artifact_placements().unwrap(), 0);
}

#[test]
fn test_artifact_placement_operations_reject_an_incompatible_table() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("peryx.redb");
    let database = redb::Database::create(&path).unwrap();
    let transaction = database.begin_write().unwrap();
    transaction
        .open_table(redb::TableDefinition::<&str, u64>::new("artifact_placement"))
        .unwrap();
    transaction.commit().unwrap();
    drop(database);
    let store = MetaStore::open_existing(path).unwrap();
    let placement = ArtifactPlacement {
        source: ArtifactSource::Hosted,
        availability: ByteAvailability::Local,
    };

    assert!(store.put_artifact_placement("digest", &placement).is_err());
    assert!(store.get_artifact_placement("digest").is_err());
    assert!(store.count_artifact_placements().is_err());
    assert!(
        store
            .list_artifact_placements(&ArtifactPlacementQuery::default())
            .is_err()
    );
    assert!(store.artifact_placement_health().is_err());
}

#[test]
fn test_artifact_placement_health_buckets_by_availability() {
    let (_dir, store) = store();
    assert_eq!(
        store.artifact_placement_health().unwrap(),
        ArtifactPlacementHealth::default()
    );

    for (digest, placement) in [
        (
            "sha256:local",
            ArtifactPlacement {
                source: ArtifactSource::Hosted,
                availability: ByteAvailability::Local,
            },
        ),
        (
            "sha256:remote",
            ArtifactPlacement {
                source: ArtifactSource::Proxy,
                availability: ByteAvailability::RemoteOnly,
            },
        ),
        (
            "sha256:gone",
            ArtifactPlacement {
                source: ArtifactSource::Hosted,
                availability: ByteAvailability::Unavailable,
            },
        ),
        (
            "sha256:generated",
            ArtifactPlacement {
                source: ArtifactSource::Generated,
                availability: ByteAvailability::Unavailable,
            },
        ),
    ] {
        store.put_artifact_placement(digest, &placement).unwrap();
    }

    assert_eq!(
        store.artifact_placement_health().unwrap(),
        ArtifactPlacementHealth {
            local: 1,
            remote_only: 1,
            unavailable: 2,
        }
    );
    assert_eq!(store.artifact_placement_health().unwrap().total(), 4);
}

#[test]
fn test_artifact_placement_trait_delegates_the_store_contract() {
    let (_dir, store) = store();
    let initial = ArtifactPlacement {
        source: ArtifactSource::Hosted,
        availability: ByteAvailability::Local,
    };
    let replacement = ArtifactPlacement {
        source: ArtifactSource::Proxy,
        availability: ByteAvailability::RemoteOnly,
    };

    <MetaStore as ArtifactPlacementStore>::put_artifact_placement(&store, "sha256:a", &initial).unwrap();
    assert_eq!(
        <MetaStore as ArtifactPlacementStore>::get_artifact_placement(&store, "sha256:a").unwrap(),
        Some(initial)
    );
    assert_eq!(
        <MetaStore as ArtifactPlacementStore>::insert_artifact_placement(&store, "sha256:a", &replacement).unwrap(),
        initial
    );
    assert!(
        <MetaStore as ArtifactPlacementStore>::compare_and_put_artifact_placement(
            &store,
            "sha256:a",
            &initial,
            &replacement,
        )
        .unwrap()
    );
    assert_eq!(
        <MetaStore as ArtifactPlacementStore>::list_artifact_placements(&store, &ArtifactPlacementQuery::default(),)
            .unwrap()
            .rows
            .len(),
        1
    );

    assert_eq!(
        <MetaStore as ArtifactPlacementStore>::artifact_placement_health(&store).unwrap(),
        ArtifactPlacementHealth {
            local: 0,
            remote_only: 1,
            unavailable: 0,
        }
    );
    assert!(<MetaStore as ArtifactPlacementStore>::delete_artifact_placement(&store, "sha256:a").unwrap());
}

#[test]
fn test_artifact_placement_insert_compare_and_pagination_edges() {
    let (_dir, store) = store();
    let placement = ArtifactPlacement {
        source: ArtifactSource::Hosted,
        availability: ByteAvailability::Local,
    };
    let replacement = ArtifactPlacement {
        source: ArtifactSource::Generated,
        availability: ByteAvailability::Unavailable,
    };

    assert_eq!(
        store.insert_artifact_placement("sha256:a", &placement).unwrap(),
        placement
    );
    assert_eq!(
        store.insert_artifact_placement("sha256:a", &replacement).unwrap(),
        placement
    );
    assert!(
        !store
            .compare_and_put_artifact_placement("sha256:a", &replacement, &replacement)
            .unwrap()
    );
    for digest in ["sha256:b", "sha256:c"] {
        store.put_artifact_placement(digest, &placement).unwrap();
    }

    for limit in [0, 101] {
        assert!(
            store
                .list_artifact_placements(&ArtifactPlacementQuery { cursor: None, limit })
                .is_err()
        );
    }
    let first = store
        .list_artifact_placements(&ArtifactPlacementQuery { cursor: None, limit: 1 })
        .unwrap();
    assert_eq!(first.rows[0].digest, "sha256:a");
    assert_eq!(first.next_cursor.as_deref(), Some("sha256:a"));
    let second = store
        .list_artifact_placements(&ArtifactPlacementQuery {
            cursor: first.next_cursor,
            limit: 2,
        })
        .unwrap();
    assert_eq!(
        second.rows.iter().map(|row| row.digest.as_str()).collect::<Vec<_>>(),
        ["sha256:b", "sha256:c"]
    );
    assert_eq!(second.next_cursor, None);
}

#[test]
fn test_count_artifact_placements_reports_the_recorded_rows() {
    let (_dir, store) = store();
    assert_eq!(store.count_artifact_placements().unwrap(), 0);

    store
        .put_artifact_placement(
            "sha256:aa",
            &ArtifactPlacement {
                source: ArtifactSource::Hosted,
                availability: ByteAvailability::Local,
            },
        )
        .unwrap();
    store
        .put_artifact_placement(
            "sha256:bb",
            &ArtifactPlacement {
                source: ArtifactSource::Proxy,
                availability: ByteAvailability::RemoteOnly,
            },
        )
        .unwrap();
    assert_eq!(store.count_artifact_placements().unwrap(), 2);

    store
        .put_artifact_placement(
            "sha256:aa",
            &ArtifactPlacement {
                source: ArtifactSource::Hosted,
                availability: ByteAvailability::Unavailable,
            },
        )
        .unwrap();
    assert_eq!(store.count_artifact_placements().unwrap(), 2);

    store.delete_artifact_placement("sha256:aa").unwrap();
    assert_eq!(store.count_artifact_placements().unwrap(), 1);
}
