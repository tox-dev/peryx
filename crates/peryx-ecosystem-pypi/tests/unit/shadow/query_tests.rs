use super::{MAX_CURSOR_BYTES, MAX_PROJECT_BYTES, ShadowQuery, ShadowQueryError};

fn query(project: &str, cursor: Option<&str>) -> ShadowQuery {
    ShadowQuery {
        repository: "images".to_owned(),
        project: project.to_owned(),
        cursor: cursor.map(str::to_owned),
        limit: 1,
    }
}

/// The bounds admit what they measure and refuse only what passes them, so a filter or cursor of
/// exactly the permitted size is still a request peryx answers. A cursor is also a position rather
/// than a value, so an empty one names nothing and is refused whatever its length.
#[test]
fn test_a_project_filter_of_exactly_the_limit_is_accepted() {
    assert_eq!(query(&"p".repeat(MAX_PROJECT_BYTES), None).validate(), Ok(()));
}

#[test]
fn test_a_project_filter_past_the_limit_is_refused() {
    assert_eq!(
        query(&"p".repeat(MAX_PROJECT_BYTES + 1), None).validate(),
        Err(ShadowQueryError::ProjectTooLong)
    );
}

#[test]
fn test_a_cursor_of_exactly_the_limit_is_accepted() {
    assert_eq!(query("demo", Some(&"c".repeat(MAX_CURSOR_BYTES))).validate(), Ok(()));
}

#[test]
fn test_a_cursor_past_the_limit_is_refused() {
    assert_eq!(
        query("demo", Some(&"c".repeat(MAX_CURSOR_BYTES + 1))).validate(),
        Err(ShadowQueryError::InvalidCursor)
    );
}

#[test]
fn test_an_empty_cursor_names_no_position() {
    assert_eq!(query("demo", Some("")).validate(), Err(ShadowQueryError::InvalidCursor));
}
