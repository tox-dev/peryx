use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use peryx_driver::AppState;
use peryx_identity::{ServerUser, SessionSealer, UserId, UserName, UserState};
use peryx_storage::meta::MetaStore;
use rstest::rstest;

use crate::config::{
    AuthConfig, AvailabilityConfig, Config, DcMember, DcMembership, DcRole, ReplicationConfig, SecretSource,
};
use crate::server::build_state_with_plugins;
use crate::tests::support::plugins;

const CONFIGURED_KEY: &str = "a-configured-signing-key-of-32-bytes";
const EXPIRES_AT: i64 = 4_102_444_800;

fn config(dir: &Path) -> Config {
    Config {
        data_dir: dir.to_path_buf(),
        ..Config::with_plugins(&plugins())
    }
}

fn configured(dir: &Path) -> Config {
    Config {
        auth: AuthConfig {
            signing_key: Some(SecretSource::Literal(CONFIGURED_KEY.to_owned())),
            ..AuthConfig::default()
        },
        ..config(dir)
    }
}

fn build(config: &Config) -> anyhow::Result<Arc<AppState>> {
    build_state_with_plugins(config, &plugins())
}

fn key_file(dir: &Path) -> std::path::PathBuf {
    dir.join("session-key")
}

fn user() -> ServerUser {
    ServerUser {
        id: UserId::random(),
        name: UserName::new("Ada Lovelace").unwrap(),
        state: UserState::Active,
        revision: 1,
        session_epoch: 0,
    }
}

fn opens(state: &AppState, cookie: &str) -> bool {
    state
        .serving
        .session_sealer()
        .unwrap()
        .open_session(cookie, 0)
        .is_some()
}

#[test]
fn test_a_standalone_server_without_a_key_seals_sessions() {
    let dir = tempfile::tempdir().unwrap();

    let state = build(&config(dir.path())).unwrap();

    assert!(state.serving.session_sealer().is_some());
}

#[test]
fn test_a_generated_session_key_does_not_enable_the_token_realm() {
    let dir = tempfile::tempdir().unwrap();

    let state = build(&config(dir.path())).unwrap();

    assert!(state.serving.signer.is_none());
}

#[test]
fn test_the_generated_session_key_persists_in_the_data_directory() {
    let dir = tempfile::tempdir().unwrap();
    let state = build(&config(dir.path())).unwrap();

    let key = std::fs::read_to_string(key_file(dir.path())).unwrap();

    let cookie = SessionSealer::new(key.as_bytes()).seal_session(&user(), EXPIRES_AT);
    assert!(opens(&state, &cookie));
}

#[cfg(unix)]
#[test]
fn test_the_generated_session_key_is_readable_only_by_its_owner() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();

    drop(build(&config(dir.path())).unwrap());

    let mode = std::fs::metadata(key_file(dir.path())).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn test_a_restart_reuses_the_generated_session_key() {
    let dir = tempfile::tempdir().unwrap();
    let first = build(&config(dir.path())).unwrap();
    let cookie = first
        .serving
        .session_sealer()
        .unwrap()
        .seal_session(&user(), EXPIRES_AT);
    drop(first);

    let second = build(&config(dir.path())).unwrap();

    assert!(opens(&second, &cookie));
}

#[test]
fn test_a_configured_signing_key_seals_sessions_without_writing_a_key_file() {
    let dir = tempfile::tempdir().unwrap();

    let state = build(&configured(dir.path())).unwrap();

    let cookie = SessionSealer::new(CONFIGURED_KEY.as_bytes()).seal_session(&user(), EXPIRES_AT);
    assert!(opens(&state, &cookie));
    assert!(!key_file(dir.path()).exists());
}

#[test]
fn test_a_configured_signing_key_wins_over_a_generated_one() {
    let dir = tempfile::tempdir().unwrap();
    let generated = build(&config(dir.path())).unwrap();
    let cookie = generated
        .serving
        .session_sealer()
        .unwrap()
        .seal_session(&user(), EXPIRES_AT);
    drop(generated);

    let state = build(&configured(dir.path())).unwrap();

    assert!(!opens(&state, &cookie));
}

fn dc_primary(dir: &Path) -> Config {
    Config {
        writer_identity: Some("writer-a".to_owned()),
        availability: AvailabilityConfig::Dc(ReplicationConfig::Primary {
            source: "writer-a".to_owned(),
            token: SecretSource::Literal("secret".to_owned()),
        }),
        ..config(dir)
    }
}

fn ha_replica(dir: &Path) -> Config {
    MetaStore::open(dir.join("peryx.redb"))
        .unwrap()
        .claim_writer_identity("east-writer")
        .unwrap();
    let member = |node: &str, dc: &str, address: &str, role: DcRole| DcMember {
        node: node.to_owned(),
        dc: dc.to_owned(),
        address: address.to_owned(),
        role,
    };
    Config {
        writer_identity: Some("east-writer".to_owned()),
        node_identity: Some("west-replica".to_owned()),
        availability: AvailabilityConfig::Ha(ReplicationConfig::Replica {
            upstream: "https://writer.example/".to_owned(),
            token: SecretSource::Literal("secret".to_owned()),
            poll_interval: Duration::from_secs(1),
            page_size: NonZeroUsize::MIN,
        }),
        dc_membership: Some(DcMembership {
            group: "group".to_owned(),
            members: vec![
                member("east-writer", "east", "http://east:8000/", DcRole::Writer),
                member("west-replica", "west", "https://west:8443/", DcRole::Replica),
            ],
        }),
        blob: crate::config::BlobStorageConfig::S3(crate::config::S3StorageConfig {
            endpoint: "https://s3.example.com".to_owned(),
            bucket: "cache".to_owned(),
            prefix: String::new(),
            region: "us-east-1".to_owned(),
            path_style: true,
            request_timeout: Duration::from_secs(30),
            max_retries: 3,
            multipart_threshold: 16 << 20,
            part_size: 16 << 20,
            upload_concurrency: 4,
            conditional_writes: true,
            checksum_writes: true,
        }),
        ..config(dir)
    }
}

#[rstest]
#[case::dc(dc_primary)]
#[case::ha(ha_replica)]
fn test_a_replicated_node_without_a_configured_key_seals_no_sessions(#[case] availability: fn(&Path) -> Config) {
    let dir = tempfile::tempdir().unwrap();

    let state = build(&availability(dir.path())).unwrap();

    assert!(state.serving.session_sealer().is_none());
    assert!(!key_file(dir.path()).exists());
}

#[test]
fn test_a_short_session_key_file_stops_startup() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(key_file(dir.path()), "short").unwrap();

    let error = build(&config(dir.path())).err().unwrap();

    assert_eq!(
        error.to_string(),
        format!(
            "session key {} must contain at least 32 bytes; delete it to generate a new one",
            key_file(dir.path()).display()
        )
    );
}

#[test]
fn test_an_unreadable_session_key_stops_startup() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(key_file(dir.path())).unwrap();

    let error = build(&config(dir.path())).err().unwrap();

    assert_eq!(error.to_string(), "read the session key");
}

#[cfg(unix)]
#[test]
fn test_a_session_key_the_data_directory_cannot_hold_stops_startup() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    drop(build(&config(dir.path())).unwrap());
    std::fs::remove_file(key_file(dir.path())).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();

    let result = build(&config(dir.path()));

    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        result.err().unwrap().to_string(),
        format!("create session key {}", key_file(dir.path()).display())
    );
}
