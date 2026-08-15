use super::{TrashFilters, UiTrashPage, UiTrashRecord};

#[test]
fn trash_record_labels_each_state() {
    for (state, expected) in [
        ("restorable", "Restorable"),
        ("expired", "Expired"),
        ("other", "Other"),
        ("unexpected", "Unknown"),
    ] {
        assert_eq!(record(state, 0).state_label(), expected);
    }
}

#[test]
fn trash_record_formats_valid_and_invalid_timestamps() {
    let valid = record("restorable", 0);
    let before_rfc3339_year_range = record("expired", -62_167_219_201);
    let invalid = record("expired", i64::MAX);
    assert_eq!(
        (
            valid.deleted_at(),
            valid.deadline_at(),
            before_rfc3339_year_range.deleted_at(),
            before_rfc3339_year_range.deadline_at(),
            invalid.deleted_at(),
            invalid.deadline_at(),
        ),
        (
            "1970-01-01T00:00:00Z".to_owned(),
            "1970-01-01T00:00:00Z".to_owned(),
            "-62167219201".to_owned(),
            "-62167219201".to_owned(),
            i64::MAX.to_string(),
            i64::MAX.to_string(),
        )
    );
}

#[test]
fn trash_pages_default_missing_actor_to_hidden() {
    let page = serde_json::from_value::<UiTrashPage>(serde_json::json!({
        "trash": [{
            "ecosystem": "example",
            "repository": "root/hosted",
            "resource": "example",
            "artifact": null,
            "digest": null,
            "reason": null,
            "deleted_at_unix": 0,
            "deadline_unix": 1,
            "state": "restorable",
            "restorable": true
        }],
        "next_cursor": null
    }))
    .expect("trash page is valid");
    assert_eq!(
        page,
        UiTrashPage {
            trash: vec![UiTrashRecord {
                ecosystem: "example".to_owned(),
                repository: "root/hosted".to_owned(),
                resource: "example".to_owned(),
                artifact: None,
                digest: None,
                reason: None,
                actor: None,
                deleted_at_unix: 0,
                deadline_unix: 1,
                state: "restorable".to_owned(),
                restorable: true,
            }],
            next_cursor: None,
        }
    );
}

#[test]
fn trash_filters_encode_nonempty_values_and_cursor() {
    let filters = TrashFilters {
        repository: " root/private ".to_owned(),
        ecosystem: " example ".to_owned(),
        state: " restorable ".to_owned(),
        limit: " 50 ".to_owned(),
    };
    assert_eq!(
        filters.url(Some("next value")),
        "/+trash?repository=root%2Fprivate&ecosystem=example&state=restorable&limit=50&cursor=next+value"
    );
}

#[test]
fn trash_filters_keep_only_the_required_limit() {
    let filters = TrashFilters::default();
    assert_eq!(
        (filters.clone(), filters.url(None)),
        (
            TrashFilters {
                repository: String::new(),
                ecosystem: String::new(),
                state: String::new(),
                limit: "25".to_owned(),
            },
            "/+trash?limit=25".to_owned(),
        )
    );
}

fn record(state: &str, time: i64) -> UiTrashRecord {
    UiTrashRecord {
        ecosystem: "example".to_owned(),
        repository: "root/hosted".to_owned(),
        resource: "example".to_owned(),
        artifact: Some("artifact.bin".to_owned()),
        digest: None,
        reason: None,
        actor: None,
        deleted_at_unix: time,
        deadline_unix: time,
        state: state.to_owned(),
        restorable: state == "restorable",
    }
}
