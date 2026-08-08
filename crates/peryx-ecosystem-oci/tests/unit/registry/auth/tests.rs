use super::{Action, resource_scope, scope_actions};

#[test]
fn test_scope_actions_maps_each_verb() {
    assert_eq!(scope_actions("pull"), &[Action::Read]);
    assert_eq!(scope_actions("push"), &[Action::Write]);
    assert_eq!(scope_actions("delete"), &[Action::Delete]);
    assert_eq!(scope_actions("*"), &[Action::Read, Action::Write, Action::Delete]);
    assert!(scope_actions("mystery").is_empty());
}

#[test]
fn test_resource_scope_advertises_the_verbs_for_each_action() {
    assert_eq!(resource_scope("team/app", Action::Read), "repository:team/app:pull");
    assert_eq!(
        resource_scope("team/app", Action::Write),
        "repository:team/app:pull,push"
    );
    assert_eq!(
        resource_scope("team/app", Action::Delete),
        "repository:team/app:pull,delete"
    );
}
