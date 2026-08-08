use std::collections::BTreeSet;
use std::path::PathBuf;

use peryx_identity::{Action, Glob, Grant, GrantScope, Role};
use rstest::rstest;

use super::toml_config;
use crate::config::{self, AuthConfig, Config, ConfigError, IndexConfig, LdapBindConfig, SecretSource};

fn toml_error(text: &str) -> String {
    let partial = config::from_toml(PathBuf::from("x.toml"), text).unwrap();
    Config::default().apply(partial).unwrap_err().to_string()
}

fn hosted(body: &str) -> IndexConfig {
    let config = toml_config(&format!("[[index]]\nname = \"hosted\"\nhosted = true\n{body}"));
    config.indexes.into_iter().next().unwrap()
}

fn write_grant(projects: &[&str], actions: &[Action]) -> Grant {
    Grant {
        projects: projects.iter().copied().map(Glob::new).collect(),
        actions: actions.iter().copied().collect::<BTreeSet<_>>(),
    }
}

#[test]
fn test_auth_defaults_to_open_reads_and_a_five_minute_token() {
    let auth = Config::default().auth;
    assert_eq!(auth.signing_key, None);
    assert_eq!(auth.token_ttl_secs, 300);
    assert!(auth.default_anonymous_read);
    assert_eq!(auth.oidc_audience, "peryx");
    assert!(auth.trusted_publishers.is_empty());
    assert!(auth.ldap_providers.is_empty());
    assert!(auth.oidc_providers.is_empty());
}

#[test]
fn test_ldap_provider_config_resolves_service_search_and_group_mappings() {
    let config = toml_config(
        "[[auth.ldap_provider]]\nid = \"corporate\"\nurl = \"ldap://directory.example:389\"\n\
         base_dn = \"ou=people,dc=example,dc=com\"\nmode = \"service-search\"\nusername_attribute = \"uid\"\n\
         bind_dn = \"cn=service,dc=example,dc=com\"\nbind_password_env = \"LDAP_BIND_PASSWORD\"\n\
         subject_attribute = \"entryUUID\"\ndisplay_name_attribute = \"displayName\"\n\
         group_attribute = \"memberOf\"\nca_file = \"/run/secrets/ldap-ca.pem\"\nconnect_timeout_secs = 4\n\
         request_timeout_secs = 7\nmax_connections = 6\n\
         [[auth.ldap_provider.group_mapping]]\ngroup = \"cn=readers,ou=groups,dc=example,dc=com\"\n\
         role = \"repository_reader\"\nrepository = \"packages\"\n\
         [[index]]\nname = \"packages\"\nhosted = true\n",
    );
    config.validate().unwrap();
    let provider = &config.auth.ldap_providers[0];

    assert_eq!(provider.id.as_str(), "corporate");
    assert_eq!(provider.url.as_str(), "ldap://directory.example:389");
    assert_eq!(provider.connect_timeout, std::time::Duration::from_secs(4));
    assert_eq!(provider.request_timeout, std::time::Duration::from_secs(7));
    assert_eq!(provider.max_connections.get(), 6);
    assert_eq!(provider.ca_file, Some(PathBuf::from("/run/secrets/ldap-ca.pem")));
    assert!(matches!(
        &provider.bind,
        LdapBindConfig::Search {
            bind_password: SecretSource::Env(variable),
            ..
        } if variable == "LDAP_BIND_PASSWORD"
    ));
    assert_eq!(provider.group_mappings[0].role, Role::RepositoryReader);
    assert_eq!(
        provider.group_mappings[0].scope,
        GrantScope::Repository {
            name: "packages".to_owned()
        }
    );
}

#[test]
fn test_ldap_provider_config_resolves_direct_bind_defaults() {
    let config = toml_config(
        "[[auth.ldap_provider]]\nid = \"staff\"\nurl = \"ldap://directory.example\"\n\
         base_dn = \"ou=people,dc=example,dc=com\"\nmode = \"direct-bind\"\ndn_attribute = \"uid\"\n\
         subject_attribute = \"entryUUID\"\ndisplay_name_attribute = \"displayName\"\n",
    );
    let provider = &config.auth.ldap_providers[0];

    assert!(matches!(&provider.bind, LdapBindConfig::Direct { dn_attribute } if dn_attribute == "uid"));
    assert_eq!(format!("{:?}", provider.bind), "Direct { dn_attribute: \"uid\" }");
    assert_eq!(provider.connect_timeout, std::time::Duration::from_secs(3));
    assert_eq!(provider.request_timeout, std::time::Duration::from_secs(5));
    assert_eq!(provider.max_connections.get(), 8);
    assert!(provider.group_mappings.is_empty());
}

#[rstest]
#[case::missing_password(
    "bind_dn = \"cn=service,dc=example\"\n",
    "service search requires a bind password source"
)]
#[case::multiple_passwords(
    "bind_dn = \"cn=service,dc=example\"\nbind_password = \"secret\"\nbind_password_file = \"/secret\"\n",
    "set at most one of a secret, its `_file` sibling, and its `_env` sibling"
)]
#[case::empty_password(
    "bind_dn = \"cn=service,dc=example\"\nbind_password = \"\"\n",
    "service bind password must not be empty"
)]
fn test_ldap_service_search_rejects_invalid_password_sources(#[case] bind: &str, #[case] expected: &str) {
    let text = format!(
        "[[auth.ldap_provider]]\nid = \"staff\"\nurl = \"ldap://directory.example\"\n\
         base_dn = \"dc=example\"\nmode = \"service-search\"\nusername_attribute = \"uid\"\n{bind}\
         subject_attribute = \"entryUUID\"\ndisplay_name_attribute = \"displayName\"\n"
    );

    assert_eq!(toml_error(&text), format!("LDAP provider staff: {expected}"));
}

#[rstest]
#[case::provider_id("bad id", "invalid provider ID")]
#[case::connect_timeout("staff", "`connect_timeout_secs` must be positive")]
#[case::request_timeout("staff", "`request_timeout_secs` must be positive")]
#[case::connections("staff", "`max_connections` must be positive")]
fn test_ldap_provider_rejects_invalid_identity_or_bounds(#[case] id: &str, #[case] expected: &str) {
    let bound = match expected {
        "`connect_timeout_secs` must be positive" => "connect_timeout_secs = 0\n",
        "`request_timeout_secs` must be positive" => "request_timeout_secs = 0\n",
        "`max_connections` must be positive" => "max_connections = 0\n",
        _ => "",
    };
    let text = format!(
        "[[auth.ldap_provider]]\nid = \"{id}\"\nurl = \"ldap://directory.example\"\nbase_dn = \"dc=example\"\n\
         mode = \"direct-bind\"\ndn_attribute = \"uid\"\nsubject_attribute = \"entryUUID\"\n\
         display_name_attribute = \"displayName\"\n{bound}"
    );

    assert_eq!(toml_error(&text), format!("LDAP provider {id}: {expected}"));
}

#[test]
fn test_ldap_provider_ids_are_unique() {
    let provider = "[[auth.ldap_provider]]\nid = \"staff\"\nurl = \"ldap://directory.example\"\nbase_dn = \"dc=example\"\nmode = \"direct-bind\"\ndn_attribute = \"uid\"\nsubject_attribute = \"entryUUID\"\ndisplay_name_attribute = \"displayName\"\n";
    let config = toml_config(&format!("{provider}{provider}"));

    assert_eq!(
        config.validate().unwrap_err().to_string(),
        "LDAP provider staff: provider IDs must be unique"
    );
}

#[test]
fn test_ldap_group_mapping_repository_must_exist() {
    let config = toml_config(
        "[[auth.ldap_provider]]\nid = \"staff\"\nurl = \"ldap://directory.example\"\nbase_dn = \"dc=example\"\n\
         mode = \"direct-bind\"\ndn_attribute = \"uid\"\nsubject_attribute = \"entryUUID\"\n\
         display_name_attribute = \"displayName\"\n[[auth.ldap_provider.group_mapping]]\ngroup = \"readers\"\n\
         role = \"repository_reader\"\nrepository = \"missing\"\n",
    );

    assert_eq!(
        config.validate().unwrap_err().to_string(),
        "LDAP provider staff: group mapping repository must name a configured index"
    );
}

#[rstest]
#[case::invalid_url(
    "url = \"not a URL\"\nmode = \"direct-bind\"\ndn_attribute = \"uid\"\n",
    "`url` must be a valid LDAP URL"
)]
#[case::direct_missing_attribute(
    "url = \"ldap://directory.example\"\nmode = \"direct-bind\"\n",
    "direct bind requires `dn_attribute`"
)]
#[case::direct_search_field(
    "url = \"ldap://directory.example\"\nmode = \"direct-bind\"\ndn_attribute = \"uid\"\nbind_dn = \"cn=service\"\n",
    "direct bind accepts only `dn_attribute` bind fields"
)]
#[case::search_direct_field(
    "url = \"ldap://directory.example\"\nmode = \"service-search\"\ndn_attribute = \"uid\"\nbind_password = \"secret\"\n",
    "service search does not accept `dn_attribute`"
)]
#[case::search_missing_username(
    "url = \"ldap://directory.example\"\nmode = \"service-search\"\nbind_dn = \"cn=service\"\nbind_password = \"secret\"\n",
    "service search requires `username_attribute`"
)]
#[case::search_missing_dn(
    "url = \"ldap://directory.example\"\nmode = \"service-search\"\nusername_attribute = \"uid\"\nbind_password = \"secret\"\n",
    "service search requires `bind_dn`"
)]
fn test_ldap_provider_rejects_incomplete_modes(#[case] fields: &str, #[case] expected: &str) {
    let text = format!(
        "[[auth.ldap_provider]]\nid = \"staff\"\nbase_dn = \"dc=example\"\n{fields}\
         subject_attribute = \"entryUUID\"\ndisplay_name_attribute = \"displayName\"\n"
    );

    assert_eq!(toml_error(&text), format!("LDAP provider staff: {expected}"));
}

#[test]
fn test_ldap_provider_rejects_an_invalid_group_mapping() {
    let text = "[[auth.ldap_provider]]\nid = \"staff\"\nurl = \"ldap://directory.example\"\nbase_dn = \"dc=example\"\n\
                mode = \"direct-bind\"\ndn_attribute = \"uid\"\nsubject_attribute = \"entryUUID\"\n\
                display_name_attribute = \"displayName\"\n[[auth.ldap_provider.group_mapping]]\ngroup = \"\"\n\
                role = \"operator\"\n";

    assert_eq!(
        toml_error(text),
        "LDAP provider staff: group mapping has an invalid group"
    );
}

#[test]
fn test_ldap_provider_debug_redacts_literal_bind_passwords() {
    let text = "[[auth.ldap_provider]]\nid = \"staff\"\nurl = \"ldap://directory.example\"\nbase_dn = \"dc=example\"\n\
                mode = \"service-search\"\nusername_attribute = \"uid\"\nbind_dn = \"cn=service\"\n\
                bind_password = \"directory-secret\"\nsubject_attribute = \"entryUUID\"\n\
                display_name_attribute = \"displayName\"\n";
    let partial = config::from_toml(PathBuf::from("x.toml"), text).unwrap();
    let raw_debug = format!("{:?}", partial.auth);
    let resolved_debug = format!("{:?}", Config::default().apply(partial).unwrap().auth);

    assert!(raw_debug.contains("[redacted]"));
    assert!(resolved_debug.contains("[redacted]"));
    assert!(!raw_debug.contains("directory-secret"));
    assert!(!resolved_debug.contains("directory-secret"));
}

#[test]
fn test_trusted_publisher_config_resolves_and_validates() {
    let config = toml_config(
        "[auth]\nsigning_key = \"key\"\noidc_audience = \"packages.example\"\n\
         [[auth.trusted_publisher]]\nid = \"release\"\nissuer = \"https://token.actions.githubusercontent.com\"\n\
         repository = \"hosted\"\nsubject = \"repo:org/app:*\"\nprojects = [\"app\"]\n\
         [auth.trusted_publisher.claims]\nrepository_id = \"42\"\n\
         [[index]]\nname = \"hosted\"\nhosted = true\n",
    );
    config.validate().unwrap();
    assert_eq!(config.auth.oidc_audience, "packages.example");
    assert_eq!(config.auth.trusted_publishers[0].id, "release");
    assert_eq!(config.auth.trusted_publishers[0].claims["repository_id"], "42");
}

#[test]
fn test_trusted_publisher_accepts_a_writable_virtual_repository() {
    let config = toml_config(
        "[auth]\nsigning_key = \"key\"\n[[auth.trusted_publisher]]\nid = \"release\"\nissuer = \"https://issuer.example\"\nrepository = \"all\"\nsubject = \"*\"\nprojects = [\"app\"]\n[[index]]\nname = \"hosted\"\nhosted = true\n[[index]]\nname = \"all\"\nlayers = [\"hosted\"]\nupload = \"hosted\"\n",
    );
    config.validate().unwrap();
}

#[rstest]
#[case::no_signing_key(
    "[[auth.trusted_publisher]]\nid = \"release\"\nissuer = \"https://issuer.example\"\nrepository = \"hosted\"\nsubject = \"*\"\nprojects = [\"app\"]\n[[index]]\nname = \"hosted\"\nhosted = true\n",
    "auth: `signing_key` is required when trusted publishers are configured"
)]
#[case::wrong_repository(
    "[auth]\nsigning_key = \"key\"\n[[auth.trusted_publisher]]\nid = \"release\"\nissuer = \"https://issuer.example\"\nrepository = \"missing\"\nsubject = \"*\"\nprojects = [\"app\"]\n",
    "trusted publisher release: repository must name a writable index with trusted publishing support"
)]
#[case::wrong_ecosystem(
    "[auth]\nsigning_key = \"key\"\n[[auth.trusted_publisher]]\nid = \"release\"\nissuer = \"https://issuer.example\"\nrepository = \"images\"\nsubject = \"*\"\nprojects = [\"app\"]\n[[index]]\nname = \"images\"\necosystem = \"oci\"\nhosted = true\n",
    "trusted publisher release: repository must name a writable index with trusted publishing support"
)]
#[case::read_only_repository(
    "[auth]\nsigning_key = \"key\"\n[[auth.trusted_publisher]]\nid = \"release\"\nissuer = \"https://issuer.example\"\nrepository = \"cache\"\nsubject = \"*\"\nprojects = [\"app\"]\n[[index]]\nname = \"cache\"\n[[index.upstream]]\nname = \"primary\"\nurl = \"https://pypi.org/simple/\"\n",
    "trusted publisher release: repository must name a writable index with trusted publishing support"
)]
fn test_trusted_publisher_relationship_is_rejected(#[case] text: &str, #[case] expected: &str) {
    let config = toml_config(text);
    assert_eq!(config.validate().unwrap_err().to_string(), expected);
}

#[test]
fn test_trusted_publisher_ids_are_unique() {
    let publisher = "[[auth.trusted_publisher]]\nid = \"release\"\nissuer = \"https://issuer.example\"\nrepository = \"hosted\"\nsubject = \"*\"\nprojects = [\"app\"]\n";
    let config = toml_config(&format!(
        "[auth]\nsigning_key = \"key\"\n{publisher}{publisher}[[index]]\nname = \"hosted\"\nhosted = true\n"
    ));
    assert_eq!(
        config.validate().unwrap_err().to_string(),
        "trusted publisher release: publisher IDs must be unique"
    );
}

#[test]
fn test_auth_table_overlays_every_default() {
    let auth = toml_config("[auth]\nsigning_key = \"k3y\"\ntoken_ttl_secs = 60\ndefault_anonymous_read = false\n").auth;
    assert_eq!(auth.signing_key, Some(SecretSource::Literal("k3y".to_owned())));
    assert_eq!(auth.token_ttl_secs, 60);
    assert!(!auth.default_anonymous_read);
}

#[test]
fn test_the_largest_ttl_is_accepted() {
    let auth = toml_config("[auth]\ntoken_ttl_secs = 86400\n").auth;
    assert_eq!(auth.token_ttl_secs, 86_400);
}

#[test]
fn test_signing_key_reads_from_a_file() {
    let auth = toml_config("[auth]\nsigning_key_file = \"/run/secrets/key\"\n").auth;
    assert_eq!(
        auth.signing_key,
        Some(SecretSource::File(PathBuf::from("/run/secrets/key")))
    );
}

#[rstest]
#[case::two_key_sources(
    "[auth]\nsigning_key = \"k3y\"\nsigning_key_file = \"/run/secrets/key\"\n",
    "auth: set at most one of a secret and its `_file` sibling"
)]
#[case::zero_ttl("[auth]\ntoken_ttl_secs = 0\n", "auth: `token_ttl_secs` must be positive")]
#[case::over_max_ttl(
    "[auth]\ntoken_ttl_secs = 86401\n",
    "auth: `token_ttl_secs` must not exceed 86400 (one day)"
)]
#[case::empty_audience("[auth]\noidc_audience = \" \"\n", "auth: `oidc_audience` must not be empty")]
#[case::empty_publisher(
    "[[auth.trusted_publisher]]\nid = \"\"\nissuer = \"https://issuer.example\"\nrepository = \"repo\"\nsubject = \"*\"\nprojects = [\"app\"]\n",
    "auth: trusted publisher fields and project lists must not be empty"
)]
fn test_auth_table_is_rejected(#[case] text: &str, #[case] expected: &str) {
    assert_eq!(toml_error(text), expected);
}

#[test]
fn test_an_empty_secret_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    std::fs::write(&path, "\n").unwrap();
    let index = hosted(&format!(
        "[[index.access_token]]\nname = \"ci\"\nsecret_file = {:?}\nactions = [\"write\"]\n",
        path.display().to_string()
    ));

    let err = index.acl(&AuthConfig::default()).unwrap_err();
    assert!(matches!(err, ConfigError::EmptySecret { .. }), "{err}");
}

#[test]
fn test_a_missing_secret_file_is_refused() {
    let index = hosted(
        "[[index.access_token]]\nname = \"ci\"\nsecret_file = \"/nonexistent/peryx/token\"\nactions = [\"write\"]\n",
    );
    let err = index.acl(&AuthConfig::default()).unwrap_err();
    assert!(matches!(err, ConfigError::Read { .. }), "{err}");
}

#[test]
fn test_a_named_token_carries_its_globs_actions_and_expiry() {
    let index = hosted(
        "[[index.access_token]]\nname = \"ci\"\nsecret = \"s3cret\"\nprojects = [\"team/*\"]\n\
         actions = [\"read\", \"write\"]\nexpires_at = \"2027-01-01T00:00:00Z\"\n",
    );

    let acl = index.acl(&AuthConfig::default()).unwrap();
    assert_eq!(acl.tokens.len(), 1);
    assert_eq!(acl.tokens[0].name, "ci");
    assert_eq!(
        acl.tokens[0].grants,
        [write_grant(&["team/*"], &[Action::Read, Action::Write])]
    );
    assert_eq!(acl.tokens[0].expires_at, Some(1_798_761_600));
}

#[test]
fn test_a_named_token_defaults_to_the_whole_index() {
    let index = hosted("[[index.access_token]]\nname = \"ci\"\nsecret = \"s3cret\"\nactions = [\"write\"]\n");

    let acl = index.acl(&AuthConfig::default()).unwrap();
    assert_eq!(acl.tokens[0].grants, [write_grant(&["*"], &[Action::Write])]);
}

#[test]
fn test_a_named_token_reads_its_secret_from_a_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ci-token");
    std::fs::write(&path, "ci-s3cret").unwrap();
    let index = hosted(&format!(
        "[[index.access_token]]\nname = \"ci\"\nsecret_file = {:?}\nactions = [\"write\"]\n",
        path.display().to_string()
    ));

    let acl = index.acl(&AuthConfig::default()).unwrap();
    assert_eq!(acl.tokens[0].secret, "ci-s3cret");
}

#[test]
fn test_named_tokens_stack_on_one_index() {
    let index = hosted(
        "[[index.access_token]]\nname = \"deploy\"\nsecret = \"s3cret\"\nactions = [\"write\", \"delete\"]\n\
         [[index.access_token]]\nname = \"ci\"\nsecret = \"ci-s3cret\"\nactions = [\"write\"]\n",
    );

    let acl = index.acl(&AuthConfig::default()).unwrap();
    let names: Vec<&str> = acl.tokens.iter().map(|token| token.name.as_str()).collect();
    assert_eq!(names, ["deploy", "ci"]);
}

#[rstest]
#[case::unnamed(
    "name = \"\"\nsecret = \"s3cret\"\nactions = [\"write\"]\n",
    "token : token name is required"
)]
#[case::no_secret(
    "name = \"ci\"\nactions = [\"write\"]\n",
    "token ci: token needs a `secret` or a `secret_file`"
)]
#[case::empty_secret(
    "name = \"ci\"\nsecret = \"\"\nactions = [\"write\"]\n",
    "token ci: `secret` must not be empty"
)]
#[case::two_secret_sources(
    "name = \"ci\"\nsecret = \"s3cret\"\nsecret_file = \"/run/secrets/ci\"\nactions = [\"write\"]\n",
    "token ci: set at most one of a secret and its `_file` sibling"
)]
#[case::no_actions("name = \"ci\"\nsecret = \"s3cret\"\n", "token ci: token needs at least one action")]
#[case::bad_expiry(
    "name = \"ci\"\nsecret = \"s3cret\"\nactions = [\"write\"]\nexpires_at = \"tomorrow\"\n",
    "token ci: `expires_at` must be an RFC 3339 timestamp, for example 2027-01-01T00:00:00Z"
)]
fn test_a_named_token_is_rejected(#[case] body: &str, #[case] expected: &str) {
    let text = format!("[[index]]\nname = \"store\"\nhosted = true\n[[index.access_token]]\n{body}");
    assert_eq!(toml_error(&text), format!("index store: {expected}"));
}

#[test]
fn test_two_tokens_may_not_share_a_name() {
    let text = "[[index]]\nname = \"store\"\nhosted = true\n\
        [[index.access_token]]\nname = \"ci\"\nsecret = \"one\"\nactions = [\"write\"]\n\
        [[index.access_token]]\nname = \"ci\"\nsecret = \"two\"\nactions = [\"write\"]\n";
    assert_eq!(toml_error(text), "index store: token ci: duplicate token name");
}

#[test]
fn test_anonymous_read_defaults_to_open_and_the_index_may_close_it() {
    let open = hosted("").acl(&AuthConfig::default()).unwrap();
    let closed = hosted("anonymous_read = false\n").acl(&AuthConfig::default()).unwrap();

    assert!(open.anonymous_read);
    assert!(!closed.anonymous_read);
}

#[test]
fn test_default_anonymous_read_closes_every_index_that_does_not_open_itself() {
    let auth = AuthConfig {
        default_anonymous_read: false,
        ..AuthConfig::default()
    };

    assert!(!hosted("").acl(&auth).unwrap().anonymous_read);
    assert!(hosted("anonymous_read = true\n").acl(&auth).unwrap().anonymous_read);
}

const OIDC_FULL: &str = "[auth]\nsigning_key = \"key\"\n[[auth.oidc_provider]]\nid = \"corporate\"\nissuer = \"https://idp.example/realms/main\"\n\
     client_id = \"peryx\"\nclient_secret_env = \"OIDC_SECRET\"\n\
     redirect_uri = \"https://registry.example/oidc/corporate/callback\"\nscopes = [\"openid\", \"email\", \"groups\"]\n\
     subject_claim = \"sub\"\ndisplay_name_claim = \"name\"\ngroups_claim = \"groups\"\n\
     clock_skew_secs = 30\nrequest_timeout_secs = 8\n\
     [[auth.oidc_provider.group_mapping]]\ngroup = \"registry-admins\"\nrole = \"administrator\"\n\
     [[auth.oidc_provider.group_mapping]]\ngroup = \"packagers\"\nrole = \"repository_reader\"\nrepository = \"packages\"\n\
     [[index]]\nname = \"packages\"\nhosted = true\n";

#[test]
fn test_oidc_provider_config_resolves_every_field() {
    let config = toml_config(OIDC_FULL);
    config.validate().unwrap();
    let provider = &config.auth.oidc_providers[0];

    assert_eq!(provider.id.as_str(), "corporate");
    assert_eq!(provider.issuer.as_str(), "https://idp.example/realms/main");
    assert_eq!(provider.client_id, "peryx");
    assert!(matches!(&provider.client_secret, Some(SecretSource::Env(variable)) if variable == "OIDC_SECRET"));
    assert_eq!(
        provider.redirect_uri.as_str(),
        "https://registry.example/oidc/corporate/callback"
    );
    assert_eq!(provider.scopes, ["openid", "email", "groups"]);
    assert_eq!(provider.subject_claim, "sub");
    assert_eq!(provider.display_name_claim, "name");
    assert_eq!(provider.groups_claim.as_deref(), Some("groups"));
    assert_eq!(provider.clock_skew, std::time::Duration::from_secs(30));
    assert_eq!(provider.request_timeout, std::time::Duration::from_secs(8));
    assert_eq!(provider.group_mappings[0].role, Role::Administrator);
    assert_eq!(provider.group_mappings[0].scope, GrantScope::Server);
    assert_eq!(
        provider.group_mappings[1].scope,
        GrantScope::Repository {
            name: "packages".to_owned()
        }
    );
}

#[test]
fn test_oidc_provider_config_applies_claim_and_bound_defaults() {
    let config = toml_config(
        "[[auth.oidc_provider]]\nid = \"public\"\nissuer = \"https://idp.example\"\nclient_id = \"peryx\"\n\
         redirect_uri = \"https://registry.example/callback\"\n",
    );
    let provider = &config.auth.oidc_providers[0];

    assert!(provider.client_secret.is_none());
    assert_eq!(provider.subject_claim, "sub");
    assert_eq!(provider.display_name_claim, "name");
    assert!(provider.groups_claim.is_none());
    assert!(provider.scopes.is_empty());
    assert_eq!(provider.clock_skew, std::time::Duration::from_mins(1));
    assert_eq!(provider.request_timeout, std::time::Duration::from_secs(10));
    assert!(provider.group_mappings.is_empty());
}

#[rstest]
#[case::provider_id(
    "bad id",
    "issuer = \"https://idp.example\"\nclient_id = \"peryx\"\nredirect_uri = \"https://registry.example/cb\"\n",
    "invalid provider ID"
)]
#[case::issuer_scheme(
    "web",
    "issuer = \"http://idp.example\"\nclient_id = \"peryx\"\nredirect_uri = \"https://registry.example/cb\"\n",
    "`issuer` must be an https URL"
)]
#[case::issuer_invalid(
    "web",
    "issuer = \"not a url\"\nclient_id = \"peryx\"\nredirect_uri = \"https://registry.example/cb\"\n",
    "`issuer` must be an https URL"
)]
#[case::redirect_scheme(
    "web",
    "issuer = \"https://idp.example\"\nclient_id = \"peryx\"\nredirect_uri = \"http://registry.example/cb\"\n",
    "`redirect_uri` must be an https URL"
)]
#[case::empty_client(
    "web",
    "issuer = \"https://idp.example\"\nclient_id = \"\"\nredirect_uri = \"https://registry.example/cb\"\n",
    "`client_id` must not be empty"
)]
#[case::empty_secret(
    "web",
    "issuer = \"https://idp.example\"\nclient_id = \"peryx\"\nredirect_uri = \"https://registry.example/cb\"\nclient_secret = \"\"\n",
    "`client_secret` must not be empty"
)]
#[case::multiple_secrets(
    "web",
    "issuer = \"https://idp.example\"\nclient_id = \"peryx\"\nredirect_uri = \"https://registry.example/cb\"\nclient_secret = \"a\"\nclient_secret_env = \"B\"\n",
    "set at most one of a secret, its `_file` sibling, and its `_env` sibling"
)]
#[case::empty_subject(
    "web",
    "issuer = \"https://idp.example\"\nclient_id = \"peryx\"\nredirect_uri = \"https://registry.example/cb\"\nsubject_claim = \"\"\n",
    "`subject_claim` must not be empty"
)]
#[case::empty_display(
    "web",
    "issuer = \"https://idp.example\"\nclient_id = \"peryx\"\nredirect_uri = \"https://registry.example/cb\"\ndisplay_name_claim = \"\"\n",
    "`display_name_claim` must not be empty"
)]
#[case::empty_groups(
    "web",
    "issuer = \"https://idp.example\"\nclient_id = \"peryx\"\nredirect_uri = \"https://registry.example/cb\"\ngroups_claim = \"\"\n",
    "`groups_claim` must not be empty"
)]
#[case::request_timeout(
    "web",
    "issuer = \"https://idp.example\"\nclient_id = \"peryx\"\nredirect_uri = \"https://registry.example/cb\"\nrequest_timeout_secs = 0\n",
    "`request_timeout_secs` must be positive"
)]
fn test_oidc_provider_rejects_invalid_settings(#[case] id: &str, #[case] body: &str, #[case] expected: &str) {
    let text = format!("[[auth.oidc_provider]]\nid = \"{id}\"\n{body}");

    assert_eq!(toml_error(&text), format!("OIDC provider {id}: {expected}"));
}

#[test]
fn test_oidc_provider_ids_are_unique() {
    let provider = "[[auth.oidc_provider]]\nid = \"corporate\"\nissuer = \"https://idp.example\"\n\
         client_id = \"peryx\"\nredirect_uri = \"https://registry.example/callback\"\n";
    let config = toml_config(&format!("[auth]\nsigning_key = \"key\"\n{provider}{provider}"));

    assert_eq!(
        config.validate().unwrap_err().to_string(),
        "OIDC provider corporate: provider IDs must be unique"
    );
}

#[test]
fn test_oidc_providers_require_a_signing_key() {
    // Browser login seals its session cookie with a key derived from the token-realm signing key, so a
    // provider without one cannot mint sessions; the config layer rejects it up front.
    let config = toml_config(
        "[[auth.oidc_provider]]\nid = \"corporate\"\nissuer = \"https://idp.example\"\nclient_id = \"peryx\"\n\
         redirect_uri = \"https://registry.example/callback\"\n",
    );

    assert_eq!(
        config.validate().unwrap_err().to_string(),
        "auth: `signing_key` is required when OIDC login providers are configured"
    );
}

#[test]
fn test_oidc_group_mapping_repository_must_name_a_configured_index() {
    let config = toml_config(
        "[auth]\nsigning_key = \"key\"\n[[auth.oidc_provider]]\nid = \"corporate\"\nissuer = \"https://idp.example\"\nclient_id = \"peryx\"\n\
         redirect_uri = \"https://registry.example/callback\"\n\
         [[auth.oidc_provider.group_mapping]]\ngroup = \"team\"\nrole = \"repository_reader\"\nrepository = \"absent\"\n",
    );

    assert_eq!(
        config.validate().unwrap_err().to_string(),
        "OIDC provider corporate: group mapping repository must name a configured index"
    );
}

#[test]
fn test_oidc_group_mapping_rejects_an_invalid_group() {
    let text = "[[auth.oidc_provider]]\nid = \"corporate\"\nissuer = \"https://idp.example\"\nclient_id = \"peryx\"\n\
         redirect_uri = \"https://registry.example/callback\"\n\
         [[auth.oidc_provider.group_mapping]]\ngroup = \"\"\nrole = \"repository_reader\"\n";

    assert_eq!(
        toml_error(text),
        "OIDC provider corporate: group mapping has an invalid group"
    );
}

#[test]
fn test_oidc_provider_debug_redacts_the_client_secret() {
    let text = "[[auth.oidc_provider]]\nid = \"corporate\"\nissuer = \"https://idp.example\"\nclient_id = \"peryx\"\n\
                client_secret = \"top-secret\"\nredirect_uri = \"https://registry.example/callback\"\n";
    let partial = config::from_toml(PathBuf::from("x.toml"), text).unwrap();
    let raw_debug = format!("{:?}", partial.auth);
    let resolved_debug = format!("{:?}", Config::default().apply(partial).unwrap().auth.oidc_providers[0]);

    assert!(raw_debug.contains("[redacted]"));
    assert!(resolved_debug.contains("[redacted]"));
    assert!(!raw_debug.contains("top-secret"));
    assert!(!resolved_debug.contains("top-secret"));
}
