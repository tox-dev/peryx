use rstest::rstest;

use crate::{GrantScope, Resource, Role, RoleGrant, Scope, UserId, can_manage_grants, grants_permit};

fn repository(name: &str) -> Resource {
    Resource::Repository(name.to_owned())
}

fn server_grant(role: Role) -> RoleGrant {
    RoleGrant::new(UserId::random(), role, GrantScope::Server)
}

fn repository_grant(role: Role, name: &str) -> RoleGrant {
    RoleGrant::new(UserId::random(), role, GrantScope::Repository { name: name.to_owned() })
}

#[rstest]
#[case::publisher_writes_its_repository(
    repository_grant(Role::RepositoryPublisher, "team/api"),
    Scope::RepositoryWrite,
    repository("team/api"),
    true
)]
#[case::publisher_deletes_its_repository(
    repository_grant(Role::RepositoryPublisher, "team/api"),
    Scope::RepositoryDelete,
    repository("team/api"),
    true
)]
#[case::publisher_reads_its_repository(
    repository_grant(Role::RepositoryPublisher, "team/api"),
    Scope::RepositoryRead,
    repository("team/api"),
    true
)]
#[case::publisher_cannot_mutate_a_sibling(
    repository_grant(Role::RepositoryPublisher, "team/api"),
    Scope::RepositoryWrite,
    repository("team/web"),
    false
)]
#[case::publisher_cannot_delete_a_sibling(
    repository_grant(Role::RepositoryPublisher, "team/api"),
    Scope::RepositoryDelete,
    repository("team/web"),
    false
)]
#[case::publisher_cannot_read_operator_data(
    repository_grant(Role::RepositoryPublisher, "team/api"),
    Scope::OperatorRead,
    Resource::Operator,
    false
)]
#[case::reader_reads_its_repository(
    repository_grant(Role::RepositoryReader, "team/api"),
    Scope::RepositoryRead,
    repository("team/api"),
    true
)]
#[case::reader_cannot_write(
    repository_grant(Role::RepositoryReader, "team/api"),
    Scope::RepositoryWrite,
    repository("team/api"),
    false
)]
#[case::reader_cannot_delete(
    repository_grant(Role::RepositoryReader, "team/api"),
    Scope::RepositoryDelete,
    repository("team/api"),
    false
)]
#[case::reader_cannot_read_a_sibling(
    repository_grant(Role::RepositoryReader, "team/api"),
    Scope::RepositoryRead,
    repository("team/web"),
    false
)]
#[case::operator_reads_operator_data(server_grant(Role::Operator), Scope::OperatorRead, Resource::Operator, true)]
#[case::operator_reads_analytics(server_grant(Role::Operator), Scope::AnalyticsRead, Resource::Operator, true)]
#[case::operator_cannot_read_administration(
    server_grant(Role::Operator),
    Scope::AdministrationRead,
    Resource::Operator,
    false
)]
#[case::operator_cannot_write_administration(
    server_grant(Role::Operator),
    Scope::AdministrationWrite,
    Resource::Operator,
    false
)]
#[case::operator_cannot_read_a_repository(
    server_grant(Role::Operator),
    Scope::RepositoryRead,
    repository("team/api"),
    false
)]
#[case::operator_cannot_write_a_repository(
    server_grant(Role::Operator),
    Scope::RepositoryWrite,
    repository("team/api"),
    false
)]
#[case::administrator_writes_any_repository(
    server_grant(Role::Administrator),
    Scope::RepositoryWrite,
    repository("team/web"),
    true
)]
#[case::administrator_deletes_any_repository(
    server_grant(Role::Administrator),
    Scope::RepositoryDelete,
    repository("team/web"),
    true
)]
#[case::administrator_reads_operator_data(
    server_grant(Role::Administrator),
    Scope::OperatorRead,
    Resource::Operator,
    true
)]
#[case::administrator_reads_analytics(
    server_grant(Role::Administrator),
    Scope::AnalyticsRead,
    Resource::Operator,
    true
)]
#[case::administrator_reads_administration(
    server_grant(Role::Administrator),
    Scope::AdministrationRead,
    Resource::Operator,
    true
)]
#[case::administrator_writes_administration(
    server_grant(Role::Administrator),
    Scope::AdministrationWrite,
    Resource::Operator,
    true
)]
#[case::administrator_cannot_apply_repository_scope_to_operator_data(
    server_grant(Role::Administrator),
    Scope::RepositoryRead,
    Resource::Operator,
    false
)]
#[case::administrator_cannot_apply_operator_scope_to_a_repository(
    server_grant(Role::Administrator),
    Scope::OperatorRead,
    repository("team/api"),
    false
)]
#[case::repository_admin_writes_its_repository(
    repository_grant(Role::Administrator, "team/api"),
    Scope::RepositoryWrite,
    repository("team/api"),
    true
)]
#[case::repository_admin_cannot_reach_a_sibling(
    repository_grant(Role::Administrator, "team/api"),
    Scope::RepositoryWrite,
    repository("team/web"),
    false
)]
#[case::repository_admin_cannot_read_operator_data(
    repository_grant(Role::Administrator, "team/api"),
    Scope::OperatorRead,
    Resource::Operator,
    false
)]
#[case::server_publisher_writes_every_repository(
    server_grant(Role::RepositoryPublisher),
    Scope::RepositoryWrite,
    repository("team/web"),
    true
)]
fn test_a_single_grant_decides_the_scope_on_the_resource(
    #[case] grant: RoleGrant,
    #[case] scope: Scope,
    #[case] resource: Resource,
    #[case] expected: bool,
) {
    assert_eq!(grants_permit(std::slice::from_ref(&grant), scope, &resource), expected);
    assert_eq!(grant.permits(scope, &resource), expected);
}

#[test]
fn test_no_grant_denies_every_scope() {
    for scope in [
        Scope::RepositoryRead,
        Scope::RepositoryWrite,
        Scope::RepositoryDelete,
        Scope::OperatorRead,
        Scope::AnalyticsRead,
        Scope::AdministrationRead,
        Scope::AdministrationWrite,
    ] {
        assert!(!grants_permit(&[], scope, &repository("team/api")));
        assert!(!grants_permit(&[], scope, &Resource::Operator));
    }
}

#[test]
fn test_any_covering_grant_in_a_set_allows() {
    let grants = [
        repository_grant(Role::RepositoryReader, "team/api"),
        repository_grant(Role::RepositoryPublisher, "team/web"),
    ];

    assert!(grants_permit(&grants, Scope::RepositoryWrite, &repository("team/web")));
    assert!(grants_permit(&grants, Scope::RepositoryRead, &repository("team/api")));
    assert!(!grants_permit(&grants, Scope::RepositoryWrite, &repository("team/api")));
}

#[test]
fn test_role_scope_sets_are_fixed() {
    assert_eq!(
        Role::Administrator.scopes(),
        &[
            Scope::RepositoryRead,
            Scope::RepositoryWrite,
            Scope::RepositoryDelete,
            Scope::OperatorRead,
            Scope::AnalyticsRead,
            Scope::AdministrationRead,
            Scope::AdministrationWrite,
        ]
    );
    assert_eq!(
        Role::RepositoryPublisher.scopes(),
        &[Scope::RepositoryRead, Scope::RepositoryWrite, Scope::RepositoryDelete]
    );
    assert_eq!(Role::RepositoryReader.scopes(), &[Scope::RepositoryRead]);
    assert_eq!(Role::Operator.scopes(), &[Scope::OperatorRead, Scope::AnalyticsRead]);
    assert_eq!(
        Role::ALL,
        &[
            Role::Administrator,
            Role::RepositoryPublisher,
            Role::RepositoryReader,
            Role::Operator,
        ]
    );
}

#[test]
fn test_role_and_scope_names_are_stable() {
    assert_eq!(Role::Administrator.as_str(), "administrator");
    assert_eq!(Role::RepositoryPublisher.as_str(), "repository_publisher");
    assert_eq!(Role::RepositoryReader.as_str(), "repository_reader");
    assert_eq!(Role::Operator.as_str(), "operator");
    assert_eq!(Scope::RepositoryRead.as_str(), "repository:read");
    assert_eq!(Scope::RepositoryWrite.as_str(), "repository:write");
    assert_eq!(Scope::RepositoryDelete.as_str(), "repository:delete");
    assert_eq!(Scope::OperatorRead.as_str(), "operator:read");
    assert_eq!(Scope::AnalyticsRead.as_str(), "analytics:read");
    assert_eq!(Scope::AdministrationRead.as_str(), "administration:read");
    assert_eq!(Scope::AdministrationWrite.as_str(), "administration:write");
}

#[test]
fn test_resource_fields_name_the_class_and_value() {
    assert_eq!(repository("team/api").fields(), ("repository", "team/api"));
    assert_eq!(Resource::Operator.fields(), ("operator", ""));
}

#[rstest]
#[case(Role::Administrator, GrantScope::Server)]
#[case(Role::RepositoryPublisher, GrantScope::Repository { name: "team/api".to_owned() })]
#[case(Role::RepositoryReader, GrantScope::Repository { name: "team/web".to_owned() })]
#[case(Role::Operator, GrantScope::Server)]
fn test_role_grant_round_trips_through_json(#[case] role: Role, #[case] scope: GrantScope) {
    let grant = RoleGrant::new(UserId::random(), role, scope);
    let encoded = serde_json::to_string(&grant).unwrap();
    assert_eq!(serde_json::from_str::<RoleGrant>(&encoded).unwrap(), grant);
}

fn repository_reach(name: &str) -> GrantScope {
    GrantScope::Repository { name: name.to_owned() }
}

#[rstest]
#[case::server_admin_manages_a_repository(
    vec![server_grant(Role::Administrator)],
    repository_reach("team/api"),
    true
)]
#[case::server_admin_manages_server_grants(vec![server_grant(Role::Administrator)], GrantScope::Server, true)]
#[case::repository_admin_manages_its_repository(
    vec![repository_grant(Role::Administrator, "team/api")],
    repository_reach("team/api"),
    true
)]
#[case::repository_admin_cannot_manage_a_sibling(
    vec![repository_grant(Role::Administrator, "team/api")],
    repository_reach("team/web"),
    false
)]
#[case::repository_admin_cannot_manage_server_grants(
    vec![repository_grant(Role::Administrator, "team/api")],
    GrantScope::Server,
    false
)]
#[case::a_publisher_manages_nothing(
    vec![repository_grant(Role::RepositoryPublisher, "team/api")],
    repository_reach("team/api"),
    false
)]
#[case::a_server_operator_manages_nothing(vec![server_grant(Role::Operator)], GrantScope::Server, false)]
#[case::no_grant_manages_nothing(Vec::new(), repository_reach("team/api"), false)]
fn test_only_an_administrator_delegates_within_its_reach(
    #[case] caller: Vec<RoleGrant>,
    #[case] reach: GrantScope,
    #[case] expected: bool,
) {
    assert_eq!(can_manage_grants(&caller, &reach), expected);
}

#[rstest]
#[case::administrator_over_the_server(Role::Administrator, GrantScope::Server, true)]
#[case::operator_over_the_server(Role::Operator, GrantScope::Server, true)]
#[case::reader_over_the_server(Role::RepositoryReader, GrantScope::Server, true)]
#[case::reader_over_a_repository(
    Role::RepositoryReader,
    GrantScope::Repository { name: "team/api".to_owned() },
    true
)]
#[case::administrator_over_a_repository(
    Role::Administrator,
    GrantScope::Repository { name: "team/api".to_owned() },
    true
)]
#[case::operator_over_a_repository_is_inert(
    Role::Operator,
    GrantScope::Repository { name: "team/api".to_owned() },
    false
)]
fn test_a_binding_is_effective_only_when_it_confers_authority(
    #[case] role: Role,
    #[case] scope: GrantScope,
    #[case] expected: bool,
) {
    assert_eq!(RoleGrant::new(UserId::random(), role, scope).is_effective(), expected);
}
