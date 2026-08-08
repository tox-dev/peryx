use peryx_core::{ShadowCandidate, ShadowReason, ShadowSource};

use super::{DEFAULT_LIMIT, ShadowQuery, ShadowQueryError, paginate};

fn candidate(filename: &str, member: &str, selected: bool) -> ShadowCandidate {
    ShadowCandidate {
        repository: "root/alpha".to_owned(),
        project: "flask".to_owned(),
        member: member.to_owned(),
        source: if selected {
            ShadowSource::Hosted
        } else {
            ShadowSource::Cached
        },
        filename: filename.to_owned(),
        digest: Some("sha256:abc".to_owned()),
        selected,
        reason: (!selected).then_some(ShadowReason::Precedence),
    }
}

fn query(limit: usize) -> ShadowQuery {
    ShadowQuery {
        limit,
        ..ShadowQuery::new("root/alpha".to_owned(), "flask".to_owned())
    }
}

#[test]
fn test_new_query_carries_the_default_page_size() {
    assert_eq!(ShadowQuery::new(String::new(), String::new()).limit, DEFAULT_LIMIT);
}

#[test]
fn test_validate_rejects_bad_limit_cursor_and_project() {
    assert_eq!(query(0).validate(), Err(ShadowQueryError::InvalidLimit));
    assert_eq!(query(101).validate(), Err(ShadowQueryError::InvalidLimit));
    assert_eq!(
        ShadowQuery {
            cursor: Some(String::new()),
            ..query(25)
        }
        .validate(),
        Err(ShadowQueryError::InvalidCursor)
    );
    assert_eq!(
        ShadowQuery {
            cursor: Some("x".repeat(1_025)),
            ..query(25)
        }
        .validate(),
        Err(ShadowQueryError::InvalidCursor)
    );
    assert_eq!(
        ShadowQuery {
            project: "p".repeat(513),
            ..query(25)
        }
        .validate(),
        Err(ShadowQueryError::ProjectTooLong)
    );
    assert_eq!(query(25).validate(), Ok(()));
}

#[test]
fn test_error_messages_are_actionable() {
    assert_eq!(
        ShadowQueryError::InvalidLimit.to_string(),
        "limit must be between 1 and 100"
    );
    assert_eq!(ShadowQueryError::InvalidCursor.to_string(), "invalid shadow cursor");
    assert_eq!(
        ShadowQueryError::ProjectTooLong.to_string(),
        "project filter exceeds 512 bytes"
    );
    assert_eq!(ShadowQueryError::Store("boom".to_owned()).to_string(), "boom");
}

#[test]
fn test_paginate_orders_by_filename_then_selection() {
    let candidates = vec![
        candidate("flask-1.0.bin", "alpha", false),
        candidate("flask-1.0.bin", "hosted", true),
        candidate("flask-2.0.bin", "hosted", true),
    ];

    let page = paginate(candidates, &query(25));

    assert_eq!(page.next_cursor, None);
    let rows: Vec<(&str, &str)> = page
        .candidates
        .iter()
        .map(|candidate| (candidate.filename.as_str(), candidate.member.as_str()))
        .collect();
    assert_eq!(
        rows,
        vec![
            ("flask-1.0.bin", "hosted"),
            ("flask-1.0.bin", "alpha"),
            ("flask-2.0.bin", "hosted")
        ],
        "the selected candidate leads its filename group"
    );
}

#[test]
fn test_paginate_cursor_resumes_after_the_last_row_and_stays_stable() {
    let candidates = vec![
        candidate("a.bin", "hosted", true),
        candidate("b.bin", "hosted", true),
        candidate("c.bin", "hosted", true),
    ];

    let first = paginate(candidates.clone(), &query(2));
    assert_eq!(
        first
            .candidates
            .iter()
            .map(|candidate| candidate.filename.clone())
            .collect::<Vec<_>>(),
        vec!["a.bin", "b.bin"]
    );
    let cursor = first.next_cursor.expect("a third row remains");

    let second = paginate(
        candidates,
        &ShadowQuery {
            cursor: Some(cursor),
            ..query(2)
        },
    );
    assert_eq!(
        second
            .candidates
            .iter()
            .map(|candidate| candidate.filename.clone())
            .collect::<Vec<_>>(),
        vec!["c.bin"],
        "the resumed page holds without skipping or duplicating a candidate"
    );
    assert_eq!(second.next_cursor, None);
}
