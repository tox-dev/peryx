use std::collections::BTreeSet;
use std::path::PathBuf;

use peryx_identity::{Action, Glob, Grant, UPLOAD_TOKEN_NAME};
use rstest::rstest;

use super::toml_config;
use crate::config::{self, AuthConfig, Config, ConfigError, IndexConfig, SecretSource};

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
}

#[test]
fn test_auth_table_overlays_every_default() {
    let auth = toml_config("[auth]\nsigning_key = \"k3y\"\ntoken_ttl_secs = 60\ndefault_anonymous_read = false\n").auth;
    assert_eq!(auth.signing_key, Some(SecretSource::Literal("k3y".to_owned())));
    assert_eq!(auth.token_ttl_secs, 60);
    assert!(!auth.default_anonymous_read);
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
fn test_auth_table_is_rejected(#[case] text: &str, #[case] expected: &str) {
    assert_eq!(toml_error(text), expected);
}

#[test]
fn test_upload_token_becomes_a_write_and_delete_token() {
    let index = hosted("upload_token = \"s3cret\"\n");
    let acl = index.acl(&AuthConfig::default()).unwrap();

    assert!(acl.anonymous_read);
    assert_eq!(acl.tokens.len(), 1);
    assert_eq!(acl.tokens[0].name, UPLOAD_TOKEN_NAME);
    assert_eq!(acl.tokens[0].secret, "s3cret");
    assert_eq!(
        acl.tokens[0].grants,
        [write_grant(&["*"], &[Action::Write, Action::Delete])]
    );
    assert_eq!(acl.tokens[0].expires_at, None);
}

#[test]
fn test_upload_token_file_holds_the_secret() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    std::fs::write(&path, "file-s3cret\n").unwrap();
    let index = hosted(&format!("upload_token_file = {:?}\n", path.display().to_string()));

    let acl = index.acl(&AuthConfig::default()).unwrap();
    assert_eq!(acl.tokens[0].secret, "file-s3cret");
}

#[test]
fn test_upload_token_file_alone_makes_the_index_hosted() {
    let config = toml_config("[[index]]\nname = \"store\"\nupload_token_file = \"/run/secrets/token\"\n");
    assert!(matches!(
        &config.indexes[0].kind,
        crate::config::IndexKind::Hosted { volatile: true, .. }
    ));
}

#[test]
fn test_an_upload_token_may_not_have_two_sources() {
    let text = "[[index]]\nname = \"store\"\nupload_token = \"s3cret\"\nupload_token_file = \"/run/secrets/token\"\n";
    assert_eq!(
        toml_error(text),
        "index store: set at most one of a secret and its `_file` sibling"
    );
}

#[test]
fn test_an_empty_secret_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    std::fs::write(&path, "\n").unwrap();
    let index = hosted(&format!("upload_token_file = {:?}\n", path.display().to_string()));

    let err = index.acl(&AuthConfig::default()).unwrap_err();
    assert!(matches!(err, ConfigError::EmptySecret { .. }), "{err}");
}

#[test]
fn test_a_missing_secret_file_is_refused() {
    let index = hosted("upload_token_file = \"/nonexistent/peryx/token\"\n");
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
fn test_the_upload_token_and_named_tokens_live_side_by_side() {
    let index = hosted(
        "upload_token = \"s3cret\"\n\
         [[index.access_token]]\nname = \"ci\"\nsecret = \"ci-s3cret\"\nactions = [\"write\"]\n",
    );

    let acl = index.acl(&AuthConfig::default()).unwrap();
    let names: Vec<&str> = acl.tokens.iter().map(|token| token.name.as_str()).collect();
    assert_eq!(names, [UPLOAD_TOKEN_NAME, "ci"]);
}

#[rstest]
#[case::unnamed(
    "name = \"\"\nsecret = \"s3cret\"\nactions = [\"write\"]\n",
    "token : token name is required"
)]
#[case::reserved(
    "name = \"upload_token\"\nsecret = \"s3cret\"\nactions = [\"write\"]\n",
    "token upload_token: token name is reserved for the `upload_token` shorthand"
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
