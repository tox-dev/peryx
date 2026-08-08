use rstest::rstest;

use super::{UiShadowCandidate, UiShadowDecision};

fn candidate(source: &str, selected: bool, reason: Option<&str>) -> UiShadowCandidate {
    UiShadowCandidate {
        member: "hosted".to_owned(),
        source: source.to_owned(),
        filename: "flask-1.0.bin".to_owned(),
        digest: Some("sha256:abc".to_owned()),
        selected,
        reason: reason.map(str::to_owned),
        decision: None,
    }
}

fn decision(state: &str, fresh: bool, next_eligible_at_unix: Option<i64>) -> UiShadowDecision {
    UiShadowDecision {
        state: state.to_owned(),
        rule: Some("blocked-project".to_owned()),
        reason: Some("project is blocked".to_owned()),
        evaluated_at_unix: 0,
        next_eligible_at_unix,
        fresh,
    }
}

#[rstest]
#[case::selected(true, None, "Selected", "selected")]
#[case::shadowed(false, Some("precedence"), "Shadowed", "shadowed")]
fn test_outcome_names_each_side(
    #[case] selected: bool,
    #[case] reason: Option<&str>,
    #[case] outcome: &str,
    #[case] key: &str,
) {
    let candidate = candidate("hosted", selected, reason);
    assert_eq!((candidate.outcome(), candidate.outcome_key()), (outcome, key));
}

#[rstest]
#[case::hosted("hosted", "hosted upload")]
#[case::cached("cached", "cached upstream")]
#[case::unknown("mirror", "mirror")]
fn test_source_text_states_each_member_class(#[case] source: &str, #[case] text: &str) {
    assert_eq!(candidate(source, true, None).source_text(), text);
}

#[rstest]
#[case::selected(None, "-")]
#[case::precedence(Some("precedence"), "Higher-precedence member")]
#[case::fallback(Some("fallback"), "Excluded by fallback policy")]
#[case::unknown(Some("other"), "other")]
fn test_reason_text_explains_each_exclusion(#[case] reason: Option<&str>, #[case] text: &str) {
    assert_eq!(candidate("cached", reason.is_none(), reason).reason_text(), text);
}

#[test]
fn test_digest_text_falls_back_to_a_dash() {
    let mut candidate = candidate("hosted", true, None);
    candidate.digest = None;
    assert_eq!(candidate.digest_text(), "-");
}

#[rstest]
#[case::allow("allow", true, "Allowed")]
#[case::deny("deny", true, "Denied")]
#[case::wait("wait", true, "Waiting")]
#[case::unknown("skip", true, "Unknown")]
#[case::stale("deny", false, "Stale Denied")]
fn test_decision_status_names_each_outcome(#[case] state: &str, #[case] fresh: bool, #[case] status: &str) {
    assert_eq!(decision(state, fresh, None).status(), status);
}

#[test]
fn test_decision_state_key_exposes_the_raw_state_for_styling() {
    assert_eq!(decision("wait", true, None).state_key(), "wait");
}

#[test]
fn test_decision_times_format_as_utc_instants() {
    let decision = decision("wait", true, Some(60));
    assert_eq!(decision.evaluated_at(), "1970-01-01T00:00:00Z");
    assert_eq!(decision.next_eligible_at(), "1970-01-01T00:01:00Z");
}

#[test]
fn test_next_eligible_falls_back_to_a_dash_without_a_retry_window() {
    assert_eq!(decision("deny", true, None).next_eligible_at(), "-");
}

#[test]
fn test_filters_build_a_query_from_the_trimmed_names() {
    let filters = super::ShadowInspectionFilters {
        repository: "  root/alpha ".to_owned(),
        project: " flask ".to_owned(),
        limit: "50".to_owned(),
    };
    assert_eq!(
        filters.url(Some("flask-1.0.bin\u{1f}0\u{1f}hosted")).unwrap(),
        "/+shadow/candidates?repository=root%2Falpha&project=flask&limit=50&cursor=flask-1.0.bin%1F0%1Fhosted",
    );
}

#[rstest]
#[case::no_repository("", "flask")]
#[case::no_project("root/alpha", "")]
fn test_filters_reject_a_blank_name(#[case] repository: &str, #[case] project: &str) {
    let filters = super::ShadowInspectionFilters {
        repository: repository.to_owned(),
        project: project.to_owned(),
        limit: "25".to_owned(),
    };
    assert_eq!(
        filters.url(None),
        Err("Enter a repository and a project to inspect.".to_owned())
    );
}
