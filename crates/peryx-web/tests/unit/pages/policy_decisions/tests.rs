use leptos::prelude::*;

use crate::model::UiPolicyDecisionPage;

use super::{PolicyDecisions, policy_decision_page, policy_decision_results};

#[test]
fn policy_page_renders_query_controls() {
    let owner = Owner::new();
    owner.set();
    let html = view! { <PolicyDecisions /> }.to_html();
    for expected in [
        "Policy decisions",
        r#"id="policy-state""#,
        "Enter credentials and search",
    ] {
        assert!(html.contains(expected), "missing {expected:?} in {html}");
    }
}

#[test]
fn policy_page_renders_result_states() {
    let page: UiPolicyDecisionPage = serde_json::from_value(serde_json::json!({
        "decisions": [{
            "id": "decision-1", "repository": "private", "resource": "example", "group": null,
            "artifact": null, "source": null, "action": "serve", "state": "allow", "rule": null,
            "reason": null, "evaluated_at_unix": 0, "input_generation": {"repository": 0},
            "next_eligible_at_unix": null, "fresh": true
        }],
        "next_cursor": "next"
    }))
    .expect("policy decision page is valid");
    let html = policy_decision_page(page).to_html();
    for expected in [
        "Loaded 1 policy decisions.",
        r#"class="badge decision-allow">Allowed</span>"#,
        "<code>private</code>",
        "<td>-</td>",
    ] {
        assert!(html.contains(expected), "missing {expected:?} in {html}");
    }

    for (loading, result, expected) in [
        (true, None, "Loading policy decisions..."),
        (false, None, "Enter credentials and search to load decisions."),
        (false, Some(Err("denied".to_owned())), "denied"),
        (
            false,
            Some(Ok(UiPolicyDecisionPage {
                decisions: Vec::new(),
                next_cursor: None,
            })),
            "No policy decisions matched",
        ),
    ] {
        let html = policy_decision_results(loading, result).to_html();
        assert!(html.contains(expected), "missing {expected:?} in {html}");
    }
}
