use std::error::Error as _;

use peryx_identity::UserId;
use rstest::rstest;
use serde_json::json;

use crate::meta::{
    CreateRepositoryError, DesiredRepository, MetaError, MetaStore, NewRepository, ReconcileAction,
    ReconcileRepositoryError, RepositoryFieldError, RepositoryQuery, RepositoryQueryError, RepositoryState,
    RepositoryStateError, RepositoryUpdate, UpdateRepositoryError,
};

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, store)
}

fn new_repo(route: &str, display_name: &str, ecosystem: &str, actor: &UserId) -> NewRepository {
    NewRepository {
        route: route.to_owned(),
        display_name: display_name.to_owned(),
        ecosystem: ecosystem.to_owned(),
        definition: json!({"kind": "hosted"}),
        created_by: actor.clone(),
    }
}

#[test]
fn test_create_repository_assigns_a_stable_enabled_version_one_record() {
    let (_dir, store) = store();
    let actor = UserId::random();
    let record = store
        .create_repository(new_repo("team/alpha", "Team ecosystem A", "alpha", &actor), 100)
        .unwrap();

    assert!(record.id.as_str().starts_with("repo_"));
    assert_eq!(record.route, "team/alpha");
    assert_eq!(record.display_name, "Team ecosystem A");
    assert_eq!(record.ecosystem, "alpha");
    assert_eq!(record.definition, json!({"kind": "hosted"}));
    assert_eq!(record.state, RepositoryState::Enabled);
    assert_eq!(record.version, 1);
    assert_eq!(record.created_by, actor);
    assert_eq!(record.updated_by, actor);
    assert_eq!(record.created_at_unix, 100);
    assert_eq!(record.updated_at_unix, 100);

    assert_eq!(store.repository(&record.id).unwrap(), Some(record.clone()));
    assert_eq!(store.repository_by_route("team/alpha").unwrap(), Some(record));
}

#[test]
fn test_repository_lookup_misses_are_absent() {
    let (_dir, store) = store();
    assert_eq!(store.repository(&crate::meta::RepositoryId::random()).unwrap(), None);
    assert_eq!(store.repository_by_route("no-such-route").unwrap(), None);
}

#[test]
fn test_create_repository_rejects_a_duplicate_route() {
    let (_dir, store) = store();
    let actor = UserId::random();
    store
        .create_repository(new_repo("shared", "First", "alpha", &actor), 1)
        .unwrap();
    assert!(matches!(
        store.create_repository(new_repo("shared", "Second", "beta", &actor), 2),
        Err(CreateRepositoryError::DuplicateRoute { route }) if route == "shared"
    ));
    // The losing create leaves the first record untouched.
    assert_eq!(
        store.repository_by_route("shared").unwrap().unwrap().display_name,
        "First"
    );
}

#[rstest]
#[case::empty_route("", "name", "alpha", RepositoryFieldError::EmptyRoute)]
#[case::long_route(&"r".repeat(513), "name", "alpha", RepositoryFieldError::RouteTooLong)]
#[case::empty_name("route", "", "alpha", RepositoryFieldError::EmptyDisplayName)]
#[case::long_name("route", &"n".repeat(257), "alpha", RepositoryFieldError::DisplayNameTooLong)]
#[case::empty_ecosystem("route", "name", "", RepositoryFieldError::EmptyEcosystem)]
#[case::long_ecosystem("route", "name", &"e".repeat(65), RepositoryFieldError::EcosystemTooLong)]
fn test_create_repository_validates_every_field(
    #[case] route: &str,
    #[case] display_name: &str,
    #[case] ecosystem: &str,
    #[case] expected: RepositoryFieldError,
) {
    let (_dir, store) = store();
    let error = store
        .create_repository(new_repo(route, display_name, ecosystem, &UserId::random()), 1)
        .unwrap_err();
    assert!(matches!(error, CreateRepositoryError::Field(field) if field == expected));
    assert!(!expected.to_string().is_empty());
}

#[test]
fn test_update_repository_preserves_identity_and_advances_the_version() {
    let (_dir, store) = store();
    let creator = UserId::random();
    let editor = UserId::random();
    let record = store
        .create_repository(new_repo("team/alpha", "Old Name", "alpha", &creator), 10)
        .unwrap();

    let updated = store
        .update_repository(
            &record.id,
            1,
            RepositoryUpdate {
                display_name: "New Name".to_owned(),
                definition: json!({"kind": "hosted", "volatile": true}),
            },
            &editor,
            20,
        )
        .unwrap();

    assert_eq!(updated.id, record.id);
    assert_eq!(updated.route, "team/alpha");
    assert_eq!(updated.ecosystem, "alpha");
    assert_eq!(updated.display_name, "New Name");
    assert_eq!(updated.definition, json!({"kind": "hosted", "volatile": true}));
    assert_eq!(updated.version, 2);
    assert_eq!(updated.created_by, creator);
    assert_eq!(updated.created_at_unix, 10);
    assert_eq!(updated.updated_by, editor);
    assert_eq!(updated.updated_at_unix, 20);
    assert_eq!(store.repository(&record.id).unwrap(), Some(updated));
}

#[test]
fn test_update_repository_rejects_a_missing_record_and_an_invalid_name() {
    let (_dir, store) = store();
    let record = store
        .create_repository(new_repo("r", "R", "alpha", &UserId::random()), 1)
        .unwrap();
    assert!(matches!(
        store.update_repository(
            &crate::meta::RepositoryId::random(),
            1,
            RepositoryUpdate {
                display_name: "X".to_owned(),
                definition: json!({})
            },
            &UserId::random(),
            2,
        ),
        Err(UpdateRepositoryError::NotFound)
    ));
    assert!(matches!(
        store.update_repository(
            &record.id,
            1,
            RepositoryUpdate {
                display_name: String::new(),
                definition: json!({})
            },
            &UserId::random(),
            2,
        ),
        Err(UpdateRepositoryError::Field(RepositoryFieldError::EmptyDisplayName))
    ));
}

#[test]
fn test_update_repository_conflict_preserves_the_winning_update() {
    let (_dir, store) = store();
    let actor = UserId::random();
    let record = store
        .create_repository(new_repo("r", "Base", "alpha", &actor), 1)
        .unwrap();
    let winner = store
        .update_repository(
            &record.id,
            1,
            RepositoryUpdate {
                display_name: "Winner".to_owned(),
                definition: json!({"w": 1}),
            },
            &actor,
            2,
        )
        .unwrap();
    assert_eq!(winner.version, 2);

    let conflict = store
        .update_repository(
            &record.id,
            1,
            RepositoryUpdate {
                display_name: "Loser".to_owned(),
                definition: json!({"l": 1}),
            },
            &actor,
            3,
        )
        .unwrap_err();
    assert!(matches!(
        conflict,
        UpdateRepositoryError::VersionConflict { current: 2 }
    ));
    // The winning update survives the stale attempt.
    assert_eq!(store.repository(&record.id).unwrap(), Some(winner));
}

#[test]
fn test_set_repository_enabled_toggles_state_and_is_idempotent() {
    let (_dir, store) = store();
    let actor = UserId::random();
    let record = store.create_repository(new_repo("r", "R", "alpha", &actor), 1).unwrap();

    let disabled = store.set_repository_enabled(&record.id, 1, false, &actor, 5).unwrap();
    assert_eq!(disabled.state, RepositoryState::Disabled);
    assert_eq!(disabled.version, 2);
    assert_eq!(disabled.updated_at_unix, 5);

    // Disabling again at the current version is a no-op that neither bumps the version nor the clock.
    let again = store.set_repository_enabled(&record.id, 2, false, &actor, 6).unwrap();
    assert_eq!(again.version, 2);
    assert_eq!(again.updated_at_unix, 5);

    let enabled = store.set_repository_enabled(&record.id, 2, true, &actor, 7).unwrap();
    assert_eq!(enabled.state, RepositoryState::Enabled);
    assert_eq!(enabled.version, 3);
}

#[test]
fn test_set_repository_enabled_rejects_missing_records_and_stale_preconditions() {
    let (_dir, store) = store();
    let record = store
        .create_repository(new_repo("r", "R", "alpha", &UserId::random()), 1)
        .unwrap();
    assert!(matches!(
        store.set_repository_enabled(&crate::meta::RepositoryId::random(), 1, false, &UserId::random(), 2),
        Err(RepositoryStateError::NotFound)
    ));
    assert!(matches!(
        store.set_repository_enabled(&record.id, 9, false, &UserId::random(), 2),
        Err(RepositoryStateError::VersionConflict { current: 1 })
    ));
}

#[test]
fn test_list_repositories_filters_by_state_and_paginates_stably() {
    let (_dir, store) = store();
    let actor = UserId::random();
    let mut ids = Vec::new();
    for index in 0..4 {
        let record = store
            .create_repository(new_repo(&format!("r{index}"), "R", "alpha", &actor), 1)
            .unwrap();
        ids.push(record.id);
    }
    ids.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    // Disable the record that sorts first so state filtering is observable.
    store.set_repository_enabled(&ids[0], 1, false, &actor, 2).unwrap();

    let first = store
        .list_repositories(&RepositoryQuery {
            limit: 2,
            ..RepositoryQuery::default()
        })
        .unwrap();
    assert_eq!(
        first.repositories.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
        vec![ids[0].clone(), ids[1].clone()]
    );
    assert_eq!(first.next_cursor.as_deref(), Some(ids[1].as_str()));

    let second = store
        .list_repositories(&RepositoryQuery {
            cursor: Some(ids[1].clone()),
            limit: 2,
            ..RepositoryQuery::default()
        })
        .unwrap();
    assert_eq!(
        second.repositories.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
        vec![ids[2].clone(), ids[3].clone()]
    );
    assert_eq!(second.next_cursor, None);

    let disabled = store
        .list_repositories(&RepositoryQuery {
            state: Some(RepositoryState::Disabled),
            ..RepositoryQuery::default()
        })
        .unwrap();
    assert_eq!(disabled.repositories.len(), 1);
    assert_eq!(disabled.repositories[0].id, ids[0]);

    let enabled = store
        .list_repositories(&RepositoryQuery {
            state: Some(RepositoryState::Enabled),
            ..RepositoryQuery::default()
        })
        .unwrap();
    assert_eq!(enabled.repositories.len(), 3);
}

#[test]
fn test_list_repositories_is_empty_without_records() {
    let (_dir, store) = store();
    let page = store.list_repositories(&RepositoryQuery::default()).unwrap();
    assert!(page.repositories.is_empty());
    assert_eq!(page.next_cursor, None);
}

#[rstest]
#[case(0)]
#[case(101)]
fn test_list_repositories_rejects_invalid_limits(#[case] limit: usize) {
    let (_dir, store) = store();
    assert!(matches!(
        store.list_repositories(&RepositoryQuery {
            limit,
            ..RepositoryQuery::default()
        }),
        Err(RepositoryQueryError::InvalidLimit)
    ));
}

#[test]
fn test_repository_records_survive_a_restart() {
    let (dir, store) = store();
    let record = store
        .create_repository(new_repo("keep", "Keep", "alpha", &UserId::random()), 1)
        .unwrap();
    drop(store);

    let reopened = MetaStore::open_existing(dir.path().join("peryx.redb")).unwrap();
    assert_eq!(reopened.repository(&record.id).unwrap(), Some(record));
}

#[test]
fn test_repository_operations_surface_a_corrupt_record() {
    let (dir, store) = store();
    let record = store
        .create_repository(new_repo("team/alpha", "R", "alpha", &UserId::random()), 1)
        .unwrap();
    drop(store);
    let path = dir.path().join("peryx.redb");
    let database = redb::Database::open(&path).unwrap();
    let write = database.begin_write().unwrap();
    {
        let mut table = write
            .open_table(redb::TableDefinition::<&str, &[u8]>::new("repository"))
            .unwrap();
        table.insert(record.id.as_str(), b"not json".as_slice()).unwrap();
    }
    write.commit().unwrap();
    drop(database);
    let store = MetaStore::open_existing(path).unwrap();

    assert!(store.repository(&record.id).is_err());
    assert!(store.repository_by_route("team/alpha").is_err());
    assert!(matches!(
        store.update_repository(
            &record.id,
            1,
            RepositoryUpdate {
                display_name: "X".to_owned(),
                definition: json!({})
            },
            &UserId::random(),
            2,
        ),
        Err(UpdateRepositoryError::Store(_))
    ));
    assert!(matches!(
        store.set_repository_enabled(&record.id, 1, false, &UserId::random(), 2),
        Err(RepositoryStateError::Store(_))
    ));
    assert!(matches!(
        store.list_repositories(&RepositoryQuery::default()),
        Err(RepositoryQueryError::Store(_))
    ));
}

#[test]
fn test_repository_errors_convert_display_and_report_a_source() {
    let meta = || MetaError::DriverPrecondition("boom".to_owned());

    let create: CreateRepositoryError = meta().into();
    assert!(!create.to_string().is_empty());
    assert!(!format!("{create:?}").is_empty());
    let _ = create.source();
    let create_field: CreateRepositoryError = RepositoryFieldError::EmptyRoute.into();
    assert!(!create_field.to_string().is_empty());
    let _ = create_field.source();
    assert!(
        !CreateRepositoryError::DuplicateRoute { route: "r".to_owned() }
            .to_string()
            .is_empty()
    );

    let update: UpdateRepositoryError = meta().into();
    assert!(!update.to_string().is_empty());
    let _ = update.source();
    let update_field: UpdateRepositoryError = RepositoryFieldError::EmptyDisplayName.into();
    assert!(!update_field.to_string().is_empty());
    assert!(!UpdateRepositoryError::NotFound.to_string().is_empty());
    assert!(
        !UpdateRepositoryError::VersionConflict { current: 3 }
            .to_string()
            .is_empty()
    );

    let state: RepositoryStateError = meta().into();
    assert!(!state.to_string().is_empty());
    let _ = state.source();
    assert!(!RepositoryStateError::NotFound.to_string().is_empty());
    assert!(
        !RepositoryStateError::VersionConflict { current: 3 }
            .to_string()
            .is_empty()
    );

    let query: RepositoryQueryError = meta().into();
    assert!(!query.to_string().is_empty());
    let _ = query.source();
    assert!(!RepositoryQueryError::InvalidLimit.to_string().is_empty());

    for field in [
        RepositoryFieldError::EmptyRoute,
        RepositoryFieldError::RouteTooLong,
        RepositoryFieldError::EmptyDisplayName,
        RepositoryFieldError::DisplayNameTooLong,
        RepositoryFieldError::EmptyEcosystem,
        RepositoryFieldError::EcosystemTooLong,
    ] {
        assert!(!field.to_string().is_empty());
        assert!(!format!("{field:?}").is_empty());
    }
}

fn desired(route: &str, display_name: &str, ecosystem: &str) -> DesiredRepository {
    DesiredRepository {
        route: route.to_owned(),
        display_name: display_name.to_owned(),
        ecosystem: ecosystem.to_owned(),
        definition: json!({"kind": "hosted"}),
    }
}

#[test]
fn test_reconcile_repositories_creates_updates_and_preserves_identifiers() {
    let (_dir, store) = store();
    let actor = UserId::random();

    let first = store
        .reconcile_repositories(&[desired("a", "A", "alpha"), desired("b", "B", "beta")], &actor, 10)
        .unwrap();
    assert_eq!(
        first.iter().map(|entry| entry.action).collect::<Vec<_>>(),
        vec![ReconcileAction::Created, ReconcileAction::Created]
    );
    let id_a = first[0].record.id.clone();
    let id_b = first[1].record.id.clone();
    assert_eq!(first[0].record.version, 1);
    assert_eq!(first[0].record.created_by, actor);

    let second = store
        .reconcile_repositories(&[desired("a", "A", "alpha"), desired("b", "B", "beta")], &actor, 20)
        .unwrap();
    assert_eq!(
        second.iter().map(|entry| entry.action).collect::<Vec<_>>(),
        vec![ReconcileAction::Unchanged, ReconcileAction::Unchanged]
    );
    assert_eq!(second[0].record.id, id_a);
    assert_eq!(second[0].record.version, 1);

    let editor = UserId::random();
    let third = store
        .reconcile_repositories(
            &[
                DesiredRepository {
                    display_name: "A renamed".to_owned(),
                    ..desired("a", "A", "alpha")
                },
                desired("b", "B", "beta"),
                desired("c", "C", "alpha"),
            ],
            &editor,
            30,
        )
        .unwrap();
    assert_eq!(
        third.iter().map(|entry| entry.action).collect::<Vec<_>>(),
        vec![
            ReconcileAction::Updated,
            ReconcileAction::Unchanged,
            ReconcileAction::Created
        ]
    );
    assert_eq!(third[0].record.id, id_a);
    assert_eq!(third[0].record.display_name, "A renamed");
    assert_eq!(third[0].record.version, 2);
    assert_eq!(third[0].record.updated_by, editor);
    assert_eq!(third[0].record.updated_at_unix, 30);
    assert_eq!(third[0].record.created_at_unix, 10);
    assert_ne!(third[2].record.id, id_b);
    assert_eq!(store.repository_by_route("c").unwrap().unwrap().id, third[2].record.id);

    let fourth = store
        .reconcile_repositories(
            &[DesiredRepository {
                definition: json!({"kind": "virtual"}),
                ..desired("c", "C", "alpha")
            }],
            &editor,
            40,
        )
        .unwrap();
    assert_eq!(fourth[0].action, ReconcileAction::Updated);
    assert_eq!(fourth[0].record.definition, json!({"kind": "virtual"}));
    // Routes omitted from a reconcile keep their records untouched.
    assert_eq!(store.repository(&id_a).unwrap().unwrap().display_name, "A renamed");
}

#[test]
fn test_reconcile_repositories_rejects_duplicate_routes_in_the_batch() {
    let (_dir, store) = store();
    assert!(matches!(
        store.reconcile_repositories(&[desired("dup", "A", "alpha"), desired("dup", "B", "alpha")], &UserId::random(), 1),
        Err(ReconcileRepositoryError::DuplicateRoute { route }) if route == "dup"
    ));
    assert_eq!(store.repository_by_route("dup").unwrap(), None);
}

#[test]
fn test_reconcile_repositories_rejects_an_ecosystem_change_and_rolls_back() {
    let (_dir, store) = store();
    let actor = UserId::random();
    store
        .reconcile_repositories(&[desired("b", "B", "alpha")], &actor, 1)
        .unwrap();

    let error = store
        .reconcile_repositories(&[desired("a", "A", "beta"), desired("b", "B", "beta")], &actor, 2)
        .unwrap_err();
    assert!(matches!(
        error,
        ReconcileRepositoryError::EcosystemChanged { route, found, desired }
            if route == "b" && found == "alpha" && desired == "beta"
    ));
    // The first entry was staged in the same transaction, so the rejection rolls it back too.
    assert_eq!(store.repository_by_route("a").unwrap(), None);
    let unchanged = store.repository_by_route("b").unwrap().unwrap();
    assert_eq!(unchanged.ecosystem, "alpha");
    assert_eq!(unchanged.version, 1);
}

#[test]
fn test_reconcile_repositories_validates_fields() {
    let (_dir, store) = store();
    assert!(matches!(
        store.reconcile_repositories(&[desired("", "A", "alpha")], &UserId::random(), 1),
        Err(ReconcileRepositoryError::Field(RepositoryFieldError::EmptyRoute))
    ));
}

#[test]
fn test_reconcile_repositories_surfaces_a_corrupt_record() {
    let (dir, store) = store();
    let record = store
        .create_repository(new_repo("team/alpha", "R", "alpha", &UserId::random()), 1)
        .unwrap();
    drop(store);
    let path = dir.path().join("peryx.redb");
    let database = redb::Database::open(&path).unwrap();
    let write = database.begin_write().unwrap();
    {
        let mut table = write
            .open_table(redb::TableDefinition::<&str, &[u8]>::new("repository"))
            .unwrap();
        table.insert(record.id.as_str(), b"not json".as_slice()).unwrap();
    }
    write.commit().unwrap();
    drop(database);
    let store = MetaStore::open_existing(path).unwrap();

    assert!(matches!(
        store.reconcile_repositories(&[desired("team/alpha", "R2", "alpha")], &UserId::random(), 2),
        Err(ReconcileRepositoryError::Store(_))
    ));
}

#[test]
fn test_reconcile_types_convert_and_exercise_derives() {
    let reconcile: ReconcileRepositoryError = MetaError::DriverPrecondition("boom".to_owned()).into();
    assert!(!reconcile.to_string().is_empty());
    assert!(!format!("{reconcile:?}").is_empty());
    let _ = reconcile.source();
    let reconcile_field: ReconcileRepositoryError = RepositoryFieldError::EmptyRoute.into();
    assert!(!reconcile_field.to_string().is_empty());
    let _ = reconcile_field.source();
    assert!(
        !ReconcileRepositoryError::DuplicateRoute { route: "r".to_owned() }
            .to_string()
            .is_empty()
    );
    assert!(
        !ReconcileRepositoryError::EcosystemChanged {
            route: "r".to_owned(),
            found: "alpha".to_owned(),
            desired: "beta".to_owned(),
        }
        .to_string()
        .is_empty()
    );

    let want = desired("r", "R", "alpha");
    assert_eq!(want.clone(), want);
    assert!(format!("{want:?}").contains("DesiredRepository"));
    assert!(format!("{:?}", ReconcileAction::Created).contains("Created"));

    let (_dir, store) = store();
    let reconciled = store
        .reconcile_repositories(std::slice::from_ref(&want), &UserId::random(), 1)
        .unwrap();
    let one = reconciled[0].clone();
    assert_eq!(one, reconciled[0]);
    assert!(format!("{one:?}").contains("ReconciledRepository"));
    assert_eq!(one.action, ReconcileAction::Created);
}

#[test]
fn test_repository_types_exercise_their_derives() {
    let (_dir, store) = store();
    let record = store
        .create_repository(new_repo("r", "R", "alpha", &UserId::random()), 1)
        .unwrap();

    assert!(format!("{record:?}").contains("RepositoryRecord"));
    assert_eq!(record.clone(), record);
    assert!(record.id.to_string().starts_with("repo_"));
    assert!(format!("{:?}", record.id).contains("repo_"));
    assert!(format!("{:?}", RepositoryState::Enabled).contains("Enabled"));
    assert!(format!("{:?}", new_repo("r2", "R2", "alpha", &UserId::random())).contains("NewRepository"));
    assert!(
        format!(
            "{:?}",
            RepositoryUpdate {
                display_name: "N".to_owned(),
                definition: json!({})
            }
        )
        .contains("RepositoryUpdate")
    );

    let query = RepositoryQuery::default();
    assert_eq!(query.clone(), query);
    assert!(format!("{query:?}").contains("RepositoryQuery"));

    let page = store.list_repositories(&query).unwrap();
    assert_eq!(page.clone(), page);
    assert!(format!("{page:?}").contains("RepositoryPage"));
    let value = serde_json::to_value(&page).unwrap();
    assert!(value.get("repositories").is_some());
}
