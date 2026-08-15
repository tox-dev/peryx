use std::cell::RefCell;
use std::rc::Rc;

use peryx_identity::{
    ExternalGroup, ExternalGroupGrant, ExternalIdentity, ExternalIdentityLinker, ExternalIdentityResolution,
    ExternalLinkRequest, ExternalLogin, ExternalSubject, GrantScope, ProviderId, Role, UserName,
};
use peryx_storage::meta::MetaStore;

#[test]
fn test_external_identity_public_api_links_and_refreshes() {
    let provider = ProviderId::try_from(String::from("corporate-oidc")).unwrap();
    assert_eq!(provider.to_string(), "corporate-oidc");
    assert_eq!(String::from(provider.clone()), "corporate-oidc");
    let subject = ExternalSubject::try_from(String::from("employee-42")).unwrap();
    assert_eq!(subject.as_str(), "employee-42");
    assert_eq!(String::from(subject.clone()), "employee-42");
    assert!(!format!("{subject:?}").contains("employee-42"));
    let engineering = ExternalGroup::try_from(String::from("engineering")).unwrap();
    assert_eq!(engineering.as_str(), "engineering");
    assert_eq!(String::from(engineering.clone()), "engineering");
    assert!(!format!("{engineering:?}").contains("engineering"));
    let login = ExternalLogin::new(
        ExternalIdentity::new(provider, subject),
        UserName::new("Alice").unwrap(),
        vec![engineering.clone(), ExternalGroup::new("operators").unwrap()],
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let store = MetaStore::open(directory.path().join("peryx.redb")).unwrap();
    let linker = ExternalIdentityLinker::new(store.clone());

    let resolution = linker
        .link_or_resolve(
            &login,
            &[
                ExternalGroupGrant {
                    group: engineering,
                    role: Role::RepositoryReader,
                    scope: GrantScope::Repository {
                        name: "team/api".to_owned(),
                    },
                },
                ExternalGroupGrant {
                    group: ExternalGroup::new("operators").unwrap(),
                    role: Role::Operator,
                    scope: GrantScope::Server,
                },
            ],
        )
        .unwrap();

    assert_eq!(
        store.external_identity_user(&login.identity).unwrap(),
        Some(resolution.user.id.clone())
    );
    assert_eq!(store.user_role_grants(&resolution.user.id).unwrap().len(), 2);
    let refreshed = linker.link_or_resolve(&login, &[]).unwrap();
    assert!(!refreshed.link_created);
    assert!(refreshed.grants_changed);
    assert!(store.user_role_grants(&resolution.user.id).unwrap().is_empty());
}

#[test]
fn test_external_identity_linker_accepts_a_function_boundary() {
    let requests = Rc::new(RefCell::new(Vec::new()));
    let captured = Rc::clone(&requests);
    let linker = ExternalIdentityLinker::new(move |request| {
        captured.borrow_mut().push(request);
        Err::<ExternalIdentityResolution, _>("store unavailable")
    });
    let login = ExternalLogin::new(
        ExternalIdentity::new(
            ProviderId::new("corporate-oidc").unwrap(),
            ExternalSubject::new("employee-42").unwrap(),
        ),
        UserName::new("Alice").unwrap(),
        Vec::new(),
    )
    .unwrap();

    assert_eq!(linker.link_or_resolve(&login, &[]).unwrap_err(), "store unavailable");
    assert_eq!(
        requests.borrow().as_slice(),
        &[ExternalLinkRequest {
            identity: login.identity,
            display_name: login.display_name,
            grants: Vec::new(),
        }]
    );
}
