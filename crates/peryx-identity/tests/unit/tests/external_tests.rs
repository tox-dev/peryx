use std::cell::RefCell;
use std::rc::Rc;

use rstest::rstest;

use crate::{
    ExternalGroup, ExternalGroupGrant, ExternalIdentity, ExternalIdentityError, ExternalIdentityLinker,
    ExternalIdentityResolution, ExternalLinkRequest, ExternalLogin, ExternalSubject, GrantScope, MAX_EXTERNAL_GROUPS,
    ProviderId, Role, ServerUser, UserId, UserName, UserState,
};

fn identity(provider: &str, subject: &str) -> ExternalIdentity {
    ExternalIdentity::new(
        ProviderId::new(provider).unwrap(),
        ExternalSubject::new(subject).unwrap(),
    )
}

fn login(groups: &[&str]) -> ExternalLogin {
    ExternalLogin::new(
        identity("corporate-oidc", "employee-42"),
        UserName::new("Alice").unwrap(),
        groups.iter().map(|group| ExternalGroup::new(group).unwrap()).collect(),
    )
    .unwrap()
}

fn mapping(group: &str, role: Role, scope: GrantScope) -> ExternalGroupGrant {
    ExternalGroupGrant {
        group: ExternalGroup::new(group).unwrap(),
        role,
        scope,
    }
}

fn capturing_store(
    requests: &Rc<RefCell<Vec<ExternalLinkRequest>>>,
) -> impl Fn(ExternalLinkRequest) -> Result<ExternalIdentityResolution, ()> + use<> {
    let requests = Rc::clone(requests);
    move |request| {
        requests.borrow_mut().push(request);
        Ok(ExternalIdentityResolution {
            user: ServerUser {
                id: UserId::random(),
                name: UserName::new("Alice").unwrap(),
                state: UserState::Active,
                revision: 1,
            },
            link_created: true,
            grants_changed: true,
        })
    }
}

#[test]
fn test_provider_id_preserves_exact_safe_spelling() {
    let provider = ProviderId::new("Corp.OIDC_1").unwrap();

    assert_eq!(provider.as_str(), "Corp.OIDC_1");
    assert_eq!(provider.to_string(), "Corp.OIDC_1");
    assert_eq!(ProviderId::try_from(String::from("Corp.OIDC_1")).unwrap(), provider);
    assert_eq!(String::from(provider), "Corp.OIDC_1");
}

#[rstest]
#[case::empty("", ExternalIdentityError::EmptyProviderId)]
#[case::long(&"a".repeat(129), ExternalIdentityError::ProviderIdTooLong)]
#[case::space("corp oidc", ExternalIdentityError::InvalidProviderId)]
#[case::unicode("córp", ExternalIdentityError::InvalidProviderId)]
fn test_provider_id_rejects_invalid_values(#[case] value: &str, #[case] expected: ExternalIdentityError) {
    assert_eq!(ProviderId::new(value).unwrap_err(), expected);
    assert!(value.is_empty() || !expected.to_string().contains(value));
}

#[test]
fn test_subject_is_case_sensitive_and_not_normalized() {
    let composed = ExternalSubject::new("Élodie").unwrap();
    let decomposed = ExternalSubject::new("E\u{301}lodie").unwrap();

    assert_ne!(composed, decomposed);
    assert_ne!(
        ExternalSubject::new("Alice").unwrap(),
        ExternalSubject::new("alice").unwrap()
    );
    assert_eq!(composed.as_str(), "Élodie");
    assert_eq!(ExternalSubject::try_from(String::from("Élodie")).unwrap(), composed);
    assert_eq!(String::from(composed), "Élodie");
}

#[rstest]
#[case::empty("", ExternalIdentityError::EmptySubject)]
#[case::long(&"a".repeat(1_025), ExternalIdentityError::SubjectTooLong)]
#[case::control("subject\nvalue", ExternalIdentityError::InvalidSubject)]
fn test_subject_rejects_invalid_values(#[case] value: &str, #[case] expected: ExternalIdentityError) {
    assert_eq!(ExternalSubject::new(value).unwrap_err(), expected);
}

#[test]
fn test_subject_debug_is_redacted_through_identity_and_login() {
    let login = login(&["engineering"]);
    let debug = format!("{login:?}");

    assert!(debug.contains("[redacted]"));
    assert!(!debug.contains("employee-42"));
    assert!(!debug.contains("engineering"));
}

#[test]
fn test_group_preserves_exact_spelling() {
    let group = ExternalGroup::new("Platform Engineers").unwrap();

    assert_eq!(group.as_str(), "Platform Engineers");
    assert_eq!(
        ExternalGroup::try_from(String::from("Platform Engineers")).unwrap(),
        group
    );
    assert_eq!(String::from(group), "Platform Engineers");
}

#[rstest]
#[case::empty("", ExternalIdentityError::EmptyGroup)]
#[case::long(&"a".repeat(257), ExternalIdentityError::GroupTooLong)]
#[case::control("operators\troot", ExternalIdentityError::InvalidGroup)]
fn test_group_rejects_invalid_values(#[case] value: &str, #[case] expected: ExternalIdentityError) {
    assert_eq!(ExternalGroup::new(value).unwrap_err(), expected);
}

#[test]
fn test_external_identity_is_scoped_by_provider() {
    assert_ne!(identity("first", "same"), identity("second", "same"));
    assert_eq!(identity("first", "same"), identity("first", "same"));
}

#[test]
fn test_login_rejects_too_many_groups() {
    let groups = (0..=MAX_EXTERNAL_GROUPS)
        .map(|index| ExternalGroup::new(&format!("group-{index}")).unwrap())
        .collect();

    assert_eq!(
        ExternalLogin::new(identity("provider", "subject"), UserName::new("Alice").unwrap(), groups).unwrap_err(),
        ExternalIdentityError::TooManyGroups
    );
}

#[test]
fn test_linker_maps_known_groups_once_in_stable_order() {
    let requests = Rc::new(RefCell::new(Vec::new()));
    let linker = ExternalIdentityLinker::new(capturing_store(&requests));
    let repository = GrantScope::Repository {
        name: "team/api".to_owned(),
    };
    let mappings = [
        mapping("engineering", Role::RepositoryReader, repository.clone()),
        mapping("operators", Role::Operator, GrantScope::Server),
        mapping("engineering", Role::RepositoryReader, repository.clone()),
        mapping("unknown", Role::Administrator, GrantScope::Server),
    ];

    let resolution = linker
        .link_or_resolve(&login(&["engineering", "engineering", "operators"]), &mappings)
        .unwrap();

    assert!(resolution.link_created);
    assert_eq!(requests.borrow()[0].grants.len(), 2);
    assert_eq!(
        requests.borrow()[0].grants,
        vec![
            crate::ManagedRoleGrant {
                role: Role::Operator,
                scope: GrantScope::Server,
            },
            crate::ManagedRoleGrant {
                role: Role::RepositoryReader,
                scope: repository,
            },
        ]
    );
}

#[test]
fn test_linker_passes_an_empty_managed_set() {
    let requests = Rc::new(RefCell::new(Vec::new()));
    let linker = ExternalIdentityLinker::new(capturing_store(&requests));

    linker.link_or_resolve(&login(&[]), &[]).unwrap();

    assert!(requests.borrow()[0].grants.is_empty());
}
