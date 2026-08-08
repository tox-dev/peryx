use super::Role;

#[test]
fn test_role_string_forms_are_stable() {
    assert_eq!(Role::Cached.as_str(), "cached");
    assert_eq!(Role::Hosted.to_string(), "hosted");
    assert_eq!(Role::Virtual.as_str(), "virtual");
    assert_eq!(Role::ALL, &[Role::Cached, Role::Hosted, Role::Virtual]);
}

#[test]
fn test_role_serializes_lowercase() {
    assert_eq!(serde_json::to_string(&Role::Virtual).unwrap(), "\"virtual\"");
}
