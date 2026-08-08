use super::{ShadowCandidate, ShadowReason, ShadowSource};

fn candidate(filename: &str, member: &str, selected: bool) -> ShadowCandidate {
    ShadowCandidate {
        repository: "root/alpha".to_owned(),
        project: "flask".to_owned(),
        member: member.to_owned(),
        source: ShadowSource::Hosted,
        filename: filename.to_owned(),
        digest: Some("sha256:abc".to_owned()),
        selected,
        reason: (!selected).then_some(ShadowReason::Precedence),
    }
}

#[test]
fn test_source_as_str_names_each_member_class() {
    assert_eq!(ShadowSource::Hosted.as_str(), "hosted");
    assert_eq!(ShadowSource::Cached.as_str(), "cached");
}

#[test]
fn test_reason_as_str_names_each_exclusion() {
    assert_eq!(ShadowReason::Precedence.as_str(), "precedence");
    assert_eq!(ShadowReason::Fallback.as_str(), "fallback");
}

#[test]
fn test_cursor_orders_the_selected_candidate_before_the_shadowed_one() {
    let selected = candidate("flask-1.0.bin", "hosted", true).cursor();
    let shadowed = candidate("flask-1.0.bin", "alpha", false).cursor();
    assert!(selected < shadowed, "selected {selected:?} shadowed {shadowed:?}");
    assert_eq!(selected, "flask-1.0.bin\u{1f}0\u{1f}hosted");
}

#[test]
fn test_cursor_orders_by_filename_first() {
    let earlier = candidate("flask-1.0.bin", "z", true).cursor();
    let later = candidate("flask-2.0.bin", "a", true).cursor();
    assert!(earlier < later);
}
