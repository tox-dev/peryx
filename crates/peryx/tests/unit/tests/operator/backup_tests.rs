use std::path::PathBuf;

use peryx_ha::{ArtifactPlacement, ArtifactPlacementStore, ArtifactSource};
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;
use rstest::rstest;

use crate::config::{
    self, AvailabilityConfig, Config, DcMember, DcMembership, DcRole, ReplicationConfig, SecretSource,
};
use crate::operator;
#[cfg(unix)]
use crate::tests::support::with_blob_reference_event;
use crate::tests::support::{fixture_job, plugins, store_repositories};

#[cfg(unix)]
use super::support::staging_dir;
use super::support::{
    backup_create, backup_create_with_references, backup_fixture, bounded_before, s3_blob_config, valid_backup,
};

#[rstest]
#[case::file(false, "exists and is not a directory")]
#[case::non_empty_directory(true, "is not empty")]
fn test_backup_rejects_occupied_targets(#[case] directory: bool, #[case] expected: &str) {
    let (_source, config, _content, _metadata) = backup_fixture();
    let root = tempfile::tempdir().unwrap();
    let backup = root.path().join("backup");
    if directory {
        std::fs::create_dir(&backup).unwrap();
        std::fs::write(backup.join("blocker"), b"x").unwrap();
    } else {
        std::fs::write(&backup, b"x").unwrap();
    }

    let error = operator::backup_create(&config, &backup, &mut Vec::new()).unwrap_err();

    assert!(error.to_string().contains(expected), "{error}");
}

#[test]
fn test_backup_accepts_an_empty_precreated_target() {
    let (_source, config, _content, _metadata) = backup_fixture();
    let root = tempfile::tempdir().unwrap();
    let backup = root.path().join("backup");
    std::fs::create_dir(&backup).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&backup, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut output = Vec::new();

    backup_create_with_references(&config, &backup, &mut output).unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let metadata = std::fs::metadata(&backup).unwrap();
        assert_eq!(
            (
                metadata.uid(),
                metadata.permissions().mode() & 0o777,
                std::fs::metadata(backup.join("manifest.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
            ),
            (rustix::process::geteuid().as_raw(), 0o700, 0o600)
        );
    }
    assert_eq!(
        (
            backup.join("manifest.json").is_file(),
            String::from_utf8(output).unwrap().contains("ecosystems\tcore\n"),
        ),
        (true, true)
    );
}

#[cfg(unix)]
#[test]
fn test_backup_rejects_a_symlink_target() {
    let (_source, config, _content, _metadata) = backup_fixture();
    let root = tempfile::tempdir().unwrap();
    let actual = root.path().join("actual");
    std::fs::create_dir(&actual).unwrap();
    let backup = root.path().join("backup");
    std::os::unix::fs::symlink(&actual, &backup).unwrap();

    let error = backup_create_with_references(&config, &backup, &mut Vec::new()).unwrap_err();

    assert_eq!(
        (
            error.to_string().contains("is a symbolic link"),
            std::fs::read_dir(&actual).unwrap().next().is_none(),
        ),
        (true, true)
    );
}

#[cfg(unix)]
#[test]
fn test_backup_rejects_a_target_owned_by_another_user() {
    use std::os::unix::fs::MetadataExt as _;

    let (_source, config, _content, _metadata) = backup_fixture();
    let root = tempfile::tempdir().unwrap();
    let candidate = root.path().join("backup");
    std::fs::create_dir(&candidate).unwrap();
    let owner_changed = rustix::fs::chown(
        &candidate,
        Some(rustix::fs::Uid::from_raw(rustix::process::geteuid().as_raw() ^ 1)),
        None,
    )
    .is_ok();
    let backup = if owner_changed { candidate } else { PathBuf::from("/") };
    assert_ne!(
        std::fs::metadata(&backup).unwrap().uid(),
        rustix::process::geteuid().as_raw()
    );

    let error = backup_create_with_references(&config, &backup, &mut Vec::new()).unwrap_err();

    assert!(error.to_string().contains("is owned by another user"), "{error:#}");
}

#[cfg(unix)]
#[test]
fn test_backup_rejects_a_manifest_symlink_planted_in_the_staging_tree() {
    let (_source, config, _content, _metadata) = backup_fixture();
    let root = tempfile::tempdir().unwrap();
    let backup = root.path().join("backup");
    let victim = root.path().join("victim");
    std::fs::write(&victim, b"unchanged").unwrap();
    let event_root = root.path().to_owned();
    let event_victim = victim.clone();

    let error = with_blob_reference_event(
        move || {
            let staging = staging_dir(&event_root);
            std::os::unix::fs::symlink(event_victim, staging.join("manifest.json")).unwrap();
        },
        || backup_create_with_references(&config, &backup, &mut Vec::new()),
    )
    .unwrap_err();

    assert_eq!(
        (
            error.to_string().contains("create backup member manifest.json"),
            std::fs::read(victim).unwrap(),
            backup.exists(),
        ),
        (true, b"unchanged".to_vec(), false)
    );
}

#[cfg(unix)]
#[test]
fn test_backup_rejects_a_directory_symlink_planted_in_the_staging_tree() {
    let (_source, config, _content, _metadata) = backup_fixture();
    let root = tempfile::tempdir().unwrap();
    let backup = root.path().join("backup");
    let victim = root.path().join("victim");
    std::fs::create_dir(&victim).unwrap();
    let event_root = root.path().to_owned();
    let event_victim = victim.clone();

    let error = with_blob_reference_event(
        move || {
            let staging = staging_dir(&event_root);
            std::os::unix::fs::symlink(event_victim, staging.join("metadata")).unwrap();
        },
        || backup_create_with_references(&config, &backup, &mut Vec::new()),
    )
    .unwrap_err();

    assert_eq!(
        (
            error.to_string().contains("open backup directory metadata"),
            std::fs::read_dir(victim).unwrap().next().is_none(),
            backup.exists(),
        ),
        (true, true, false)
    );
}

#[cfg(unix)]
#[test]
fn test_backup_reports_an_unlinked_staging_directory() {
    let (_source, config, _content, _metadata) = backup_fixture();
    let root = tempfile::tempdir().unwrap();
    let backup = root.path().join("backup");
    let event_root = root.path().to_owned();

    let error = with_blob_reference_event(
        move || std::fs::remove_dir(staging_dir(&event_root)).unwrap(),
        || backup_create_with_references(&config, &backup, &mut Vec::new()),
    )
    .unwrap_err();

    assert_eq!(
        (
            error.to_string().contains("create backup directory metadata"),
            backup.exists(),
        ),
        (true, false)
    );
}

/// A concurrent attempt that publishes first owns the target; this one must fail rather than replace
/// the completed backup sitting there.
#[cfg(unix)]
#[test]
fn test_backup_refuses_to_replace_a_target_populated_after_it_was_reserved() {
    let (_source, config, _content, _metadata) = backup_fixture();
    let root = tempfile::tempdir().unwrap();
    let backup = root.path().join("backup");
    let event_backup = backup.clone();
    let mut out = Vec::new();

    let error = with_blob_reference_event(
        move || {
            std::fs::create_dir(&event_backup).unwrap();
            std::fs::write(event_backup.join("manifest.json"), b"published").unwrap();
        },
        || backup_create_with_references(&config, &backup, &mut out),
    )
    .unwrap_err();

    assert_eq!(
        (
            error.to_string().contains("publish backup to"),
            std::fs::read(backup.join("manifest.json")).unwrap(),
            out,
        ),
        (true, b"published".to_vec(), Vec::new())
    );
}

#[cfg(unix)]
#[test]
fn test_backup_reports_target_inspection_errors() {
    let (_source, config, _content, _metadata) = backup_fixture();
    let root = tempfile::tempdir().unwrap();
    let backup = root.path().join("x".repeat(1024));

    let error = backup_create_with_references(&config, &backup, &mut Vec::new()).unwrap_err();

    assert!(error.to_string().contains("inspect backup path"), "{error:#}");
}

#[test]
fn test_backup_rejects_a_live_store() {
    let (_source, config, _content, _metadata) = backup_fixture();
    let live = MetaStore::open(config.data_dir.join("peryx.redb")).unwrap();
    let root = tempfile::tempdir().unwrap();

    let error = backup_create_with_references(&config, &root.path().join("backup"), &mut Vec::new()).unwrap_err();
    drop(live);

    assert!(error.to_string().contains("is open by a running node"), "{error:#}");
}

#[test]
fn test_backup_rejects_a_missing_store() {
    let (_source, config, _content, _metadata) = backup_fixture();
    std::fs::remove_file(config.data_dir.join("peryx.redb")).unwrap();
    let root = tempfile::tempdir().unwrap();

    let error = backup_create(&config, &root.path().join("backup"), &mut Vec::new()).unwrap_err();

    assert!(format!("{error:#}").contains("read-only"), "{error:#}");
}

#[test]
fn test_backup_rejects_object_storage_before_creating_the_target() {
    let root = tempfile::tempdir().unwrap();
    let backup = root.path().join("backup");
    let config = Config {
        data_dir: root.path().join("data"),
        blob: s3_blob_config(),
        ..Config::with_plugins(&plugins())
    };

    let error = backup_create(&config, &backup, &mut Vec::new()).unwrap_err();

    assert_eq!(
        (
            error.to_string().contains("filesystem-backed repository"),
            backup.exists(),
        ),
        (true, false)
    );
}

#[test]
fn test_backup_refuses_an_uncovered_stored_ecosystem_before_creating_the_target() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = root.path().join("data");
    std::fs::create_dir(&data_dir).unwrap();
    let meta = MetaStore::open(data_dir.join("peryx.redb")).unwrap();
    store_repositories(&meta, &["pypi", "oci"]);
    drop(meta);
    let mut config = Config {
        data_dir,
        ..Config::default()
    };
    config.indexes.retain(|index| index.ecosystem.as_str() == "pypi");
    let backup = root.path().join("backup");

    let error = operator::backup_create(&config, &backup, &mut Vec::new()).unwrap_err();

    assert_eq!(
        (
            error.to_string(),
            format!("{error:#}").contains("blob-reference drivers: oci"),
            backup.exists(),
        ),
        ("scan metadata blob references".to_owned(), true, false)
    );
}

#[rstest]
#[case::missing(false, "is missing")]
#[case::tampered(true, "hashed as")]
fn test_backup_rejects_unusable_referenced_blobs(#[case] tamper: bool, #[case] expected: &str) {
    let (_source, config, content, _metadata) = backup_fixture();
    let blob = BlobStore::new(config.data_dir.join("blobs")).path_for(&content);
    if tamper {
        std::fs::write(blob, b"tampered").unwrap();
    } else {
        std::fs::remove_file(blob).unwrap();
    }

    let root = tempfile::tempdir().unwrap();
    let error = backup_create_with_references(&config, &root.path().join("backup"), &mut Vec::new()).unwrap_err();

    assert!(error.to_string().contains(expected), "{error:#}");
}

/// A failed attempt must leave the target the way it found it, so the operator retries the same
/// command instead of first cleaning up a partial tree the next attempt would refuse.
#[rstest]
#[case::absent(false)]
#[case::reserved(true)]
fn test_backup_leaves_a_retryable_target_after_a_failure(#[case] reserved: bool) {
    let (_source, config, content, _metadata) = backup_fixture();
    let blob = BlobStore::new(config.data_dir.join("blobs")).path_for(&content);
    let bytes = std::fs::read(&blob).unwrap();
    std::fs::write(&blob, b"tampered").unwrap();
    let root = tempfile::tempdir().unwrap();
    let backup = root.path().join("backup");
    if reserved {
        std::fs::create_dir(&backup).unwrap();
    }

    let error = backup_create_with_references(&config, &backup, &mut Vec::new()).unwrap_err();
    let left_behind = std::fs::read_dir(root.path()).unwrap().count();
    std::fs::write(&blob, &bytes).unwrap();
    let retried = backup_create_with_references(&config, &backup, &mut Vec::new());

    assert_eq!(
        (
            error.to_string().contains("hashed as"),
            left_behind,
            retried.is_ok(),
            backup.join("manifest.json").is_file(),
        ),
        (true, usize::from(reserved), true, true)
    );
}

#[test]
fn test_backup_creates_missing_parent_directories() {
    let (_source, config, _content, _metadata) = backup_fixture();
    let root = tempfile::tempdir().unwrap();
    let backup = root.path().join("nested").join("archive").join("backup");

    backup_create_with_references(&config, &backup, &mut Vec::new()).unwrap();

    assert!(backup.join("manifest.json").is_file());
}

#[test]
fn test_backup_records_distributed_state_and_membership() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = root.path().join("data");
    std::fs::create_dir(&data_dir).unwrap();
    let meta = MetaStore::open(data_dir.join("peryx.redb")).unwrap();
    meta.initialize_distributed_state().unwrap();
    ArtifactPlacementStore::insert_artifact_placement(
        &meta,
        "sha256:aa",
        &ArtifactPlacement::record(ArtifactSource::Hosted, true),
    )
    .unwrap();
    ArtifactPlacementStore::insert_artifact_placement(
        &meta,
        "sha256:bb",
        &ArtifactPlacement::record(ArtifactSource::Proxy, false),
    )
    .unwrap();
    let frontier = meta.current_serial().unwrap();
    drop(meta);
    let config = Config {
        data_dir,
        availability: AvailabilityConfig::Dc(ReplicationConfig::Primary {
            source: "primary-a".to_owned(),
            token: SecretSource::Literal("token".to_owned()),
        }),
        dc_membership: Some(DcMembership {
            group: "group-a".to_owned(),
            members: vec![
                DcMember {
                    node: "node-a".to_owned(),
                    dc: "east".to_owned(),
                    address: "http://10.0.0.1:8443".to_owned(),
                    role: DcRole::Writer,
                },
                DcMember {
                    node: "node-b".to_owned(),
                    dc: "west".to_owned(),
                    address: "http://10.0.0.2:8443".to_owned(),
                    role: DcRole::Replica,
                },
            ],
        }),
        ..Config::with_plugins(&plugins())
    };
    let backup = root.path().join("backup");
    let mut out = Vec::new();

    config.validate().unwrap();
    backup_create(&config, &backup, &mut out).unwrap();

    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(backup.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(
        (
            manifest["availability"]["mode"].clone(),
            manifest["availability"]["metadata_frontier"].clone(),
            manifest["availability"]["placements"].clone(),
            manifest["availability"]["membership"].clone(),
        ),
        (
            serde_json::json!("dc"),
            serde_json::json!(frontier),
            serde_json::json!(2),
            serde_json::json!({
                "group": "group-a",
                "members": [
                    {"node": "node-a", "dc": "east", "address": "http://10.0.0.1:8443", "role": "writer"},
                    {"node": "node-b", "dc": "west", "address": "http://10.0.0.2:8443", "role": "replica"}
                ]
            }),
        )
    );
    assert!(String::from_utf8(out).unwrap().contains("availability\tdc\tfrontier"));
}

#[test]
fn test_backup_propagates_summary_output_errors() {
    let fixture = valid_backup();
    let mut complete = Vec::new();
    backup_create(&fixture.config, &fixture.root.path().join("complete"), &mut complete).unwrap();
    let mut out = bounded_before(&complete, "\tplacements");

    let error = backup_create(&fixture.config, &fixture.root.path().join("failed"), &mut out).unwrap_err();

    assert_eq!(
        error.downcast_ref::<std::io::Error>().map(std::io::Error::kind),
        Some(std::io::ErrorKind::WriteZero)
    );
}

#[rstest]
#[case::manual(
    "[tls]\ncert = \"/etc/peryx/tls.crt\"\nkey = \"/etc/peryx/tls.key\"",
    "[availability]\nmode = \"dc\"\n[availability.replication]\nrole = \"primary\"\nsource = \"primary-a\"\ntoken = \"replication-token\""
)]
#[case::acme(
    "[acme]\ndomains = [\"packages.example.com\"]\ncontact = \"ops@example.com\"\ncache-dir = \"/var/cache/peryx/acme\"\nstaging = true",
    "[availability]\nmode = \"ha\"\n[availability.replication]\nrole = \"replica\"\nupstream = \"https://primary.example/\"\ntoken_file = \"/run/secrets/replication-token\"\npoll_interval_secs = 30\npage_size = 250\n[availability.listener]"
)]
fn test_backup_round_trips_complex_config(#[case] tls: &str, #[case] availability: &str) {
    let root = tempfile::tempdir().unwrap();
    let data_dir = root.path().join("data");
    std::fs::create_dir(&data_dir).unwrap();
    drop(MetaStore::open(data_dir.join("peryx.redb")).unwrap());
    let source = format!(
        r#"
host = "0.0.0.0"
port = 7443
data_dir = {data_dir:?}
netrc = "/run/secrets/upstream.netrc"
writer_identity = "writer-a"
offline = true
cache_ttl_secs = 91
hot_cache_bytes = 123456
max_stale_secs = 321

{tls}

{availability}

[log]
level = "peryx=debug"
format = "json"
sink = "file"
file = "/var/log/peryx.log"

[rate_limit]
enabled = true
max_clients = 17
trusted_proxies = ["127.0.0.1/32"]

[rate_limit.listing]
requests = 11
window_secs = 12

[rate_limit.metadata]
requests = 21
window_secs = 22

[rate_limit.artifact]
requests = 31
window_secs = 32

[rate_limit.upload]
requests = 41
window_secs = 42

[rate_limit.admin]
requests = 51
window_secs = 52

[rate_limit.authentication]
requests = 61
window_secs = 62

[auth]
signing_key_file = "/run/secrets/signing-key"
token_ttl_secs = 601
default_anonymous_read = false

[[auth.ldap_provider]]
id = "corporate"
url = "ldap://directory.example:389"
base_dn = "ou=people,dc=example,dc=com"
mode = "service-search"
username_attribute = "uid"
bind_dn = "cn=peryx,ou=services,dc=example,dc=com"
bind_password_env = "LDAP_BIND_PASSWORD"
subject_attribute = "entryUUID"
display_name_attribute = "displayName"
group_attribute = "memberOf"
ca_file = "/etc/peryx/ldap-ca.pem"
connect_timeout_secs = 4
request_timeout_secs = 7
max_connections = 6

[[auth.ldap_provider.group_mapping]]
group = "cn=publishers,ou=groups,dc=example,dc=com"
role = "repository_publisher"
repository = "main"

[[auth.ldap_provider]]
id = "partners"
url = "ldap://partners.example:389"
base_dn = "ou=people,dc=partners,dc=example"
mode = "direct-bind"
dn_attribute = "uid"
subject_attribute = "entryUUID"
display_name_attribute = "displayName"

[[auth.ldap_provider.group_mapping]]
group = "cn=operators,ou=groups,dc=partners,dc=example"
role = "operator"

[[auth.oidc_provider]]
id = "workforce"
issuer = "https://idp.example/realms/main"
client_id = "peryx"
client_secret_env = "OIDC_CLIENT_SECRET"
redirect_uri = "https://packages.example/oidc/workforce/callback"
scopes = ["openid", "groups"]
subject_claim = "sub"
display_name_claim = "name"
groups_claim = "groups"
clock_skew_secs = 45
request_timeout_secs = 8
trusted_endpoint_hosts = ["keys.idp.internal"]

[[auth.oidc_provider.group_mapping]]
group = "registry-admins"
role = "administrator"

[[index]]
name = "main"
route = "main"
ecosystem = "core"
upstream_concurrency = 7
offline = true
anonymous_read = true

[index.policy]
max_artifact_size_bytes = 8001
max_resource_size_bytes = 8002
max_accounted_bytes = 8003
max_resources = 12
quota_audit = true

[[index.upstream]]
name = "primary"
url = "https://primary.example/catalog/"
artifact_url = "https://artifacts.example/"
username = "mirror"
password = "mirror-secret"
token_env = "MIRROR_TOKEN"
credential_refresh_secs = 60
credential_refresh_on_unauthorized = false
credential_failure = "anonymous"
ca_file = "/run/secrets/ca.pem"
client_cert_file = "/run/secrets/client.pem"
client_key_file = "/run/secrets/client-key.pem"

[[index.upstream]]
name = "backup"
url = "https://backup.example/catalog/"

[index.upstream.credential_exec]
argv = ["/credential-helper"]
failure = "fail"

[[index.upstream]]
name = "refresh-fail"
url = "https://refresh.example/catalog/"
token_env = "REFRESH_TOKEN"
credential_refresh_secs = 45
credential_failure = "fail"

[[index.upstream]]
name = "exec-anonymous"
url = "https://exec.example/catalog/"

[index.upstream.credential_exec]
argv = ["/anonymous-helper"]
failure = "anonymous"

[[index]]
name = "secondary"
ecosystem = "core"
hosted = true
volatile = false
anonymous_read = false

[[index.access_token]]
name = "uploader"
secret_file = "/run/secrets/upload-token"
actions = ["write", "delete"]

[[index.access_token]]
name = "janitor"
secret = "janitor-secret"
resources = ["*"]
actions = ["delete"]
expires_at = "2027-01-01T00:00:00Z"

[[index.webhook]]
name = "audit"
url = "https://hooks.example/audit"
secret_env = "AUDIT_WEBHOOK_SECRET"
events = ["upload", "delete"]

[[index.webhook]]
name = "local"
url = "https://hooks.example/local"
secret = "webhook-secret"
events = ["upload"]

[[index]]
name = "aggregate"
ecosystem = "core"
layers = ["secondary"]
write_target = "secondary"
"#
    );
    let plugins = plugins();
    let config = Config::with_plugins(&plugins)
        .apply_with_plugins(
            config::from_toml(PathBuf::from("source.toml"), &source).unwrap(),
            &plugins,
        )
        .unwrap();
    let backup = root.path().join("backup");

    backup_create(&config, &backup, &mut Vec::new()).unwrap();
    let mut snapshot = std::fs::read_to_string(backup.join("config.toml")).unwrap();
    if matches!(config.availability, AvailabilityConfig::Ha(_)) {
        snapshot.push_str("\n[availability.listener]\n");
    }
    let restored = Config::with_plugins(&plugins)
        .apply_with_plugins(
            config::from_toml(PathBuf::from("config.toml"), &snapshot).unwrap(),
            &plugins,
        )
        .unwrap();

    assert_eq!(restored, config);
}

#[test]
fn test_backup_round_trips_job_kinds() {
    use peryx_driver::jobs::{Schedule, ScheduledJob};

    let root = tempfile::tempdir().unwrap();
    let data_dir = root.path().join("data");
    std::fs::create_dir(&data_dir).unwrap();
    drop(MetaStore::open(data_dir.join("peryx.redb")).unwrap());
    let distributed_job = |kind, settings| {
        peryx_ha_distributed::compile_scheduled_job(kind, &settings)
            .expect("distributed job kind")
            .unwrap()
    };
    let schedules = vec![
        Schedule {
            job: ScheduledJob::CacheMaintenance,
            interval: std::time::Duration::from_mins(5),
        },
        Schedule {
            job: distributed_job("reclamation", toml::Table::new()),
            interval: std::time::Duration::from_mins(10),
        },
        Schedule {
            job: distributed_job(
                "dc_copy",
                toml::Table::from_iter([("concurrency".to_owned(), toml::Value::Integer(4))]),
            ),
            interval: std::time::Duration::from_mins(2),
        },
        Schedule {
            job: ScheduledJob::Plugin(fixture_job()),
            interval: std::time::Duration::from_hours(1),
        },
    ];
    let config = Config {
        data_dir,
        availability: AvailabilityConfig::Dc(ReplicationConfig::Primary {
            source: "primary-a".to_owned(),
            token: SecretSource::Literal("token".to_owned()),
        }),
        jobs: crate::config::JobsConfig {
            mode: crate::config::JobsMode::Local,
            schedules: schedules.clone(),
        },
        ..Config::with_plugins(&plugins())
    };
    let backup = root.path().join("backup");

    backup_create(&config, &backup, &mut Vec::new()).unwrap();
    let snapshot = std::fs::read_to_string(backup.join("config.toml")).unwrap();
    let plugins = plugins();
    let restored = Config::with_plugins(&plugins)
        .apply_with_plugins(
            config::from_toml(PathBuf::from("config.toml"), &snapshot).unwrap(),
            &plugins,
        )
        .unwrap();

    assert_eq!(restored.jobs.schedules, schedules);
}

#[test]
fn test_backup_snapshots_disabled_jobs_and_log_variants() {
    use crate::config::{JobsConfig, JobsMode, LogConfig, LogFormat, LogSink};

    let root = tempfile::tempdir().unwrap();
    for (name, format, sink) in [
        ("file", LogFormat::Json, LogSink::File),
        ("journald", LogFormat::Pretty, LogSink::Journald),
        ("syslog", LogFormat::Pretty, LogSink::Syslog),
    ] {
        let data_dir = root.path().join(format!("data-{name}"));
        std::fs::create_dir(&data_dir).unwrap();
        drop(MetaStore::open(data_dir.join("peryx.redb")).unwrap());
        let backup = root.path().join(format!("backup-{name}"));
        backup_create(
            &Config {
                data_dir,
                jobs: JobsConfig {
                    mode: JobsMode::None,
                    ..JobsConfig::default()
                },
                log: LogConfig {
                    format,
                    sink,
                    file: Some(root.path().join("peryx.log")),
                    ..LogConfig::default()
                },
                ..Config::with_plugins(&plugins())
            },
            &backup,
            &mut Vec::new(),
        )
        .unwrap();
        let snapshot = std::fs::read_to_string(backup.join("config.toml")).unwrap();
        assert!(snapshot.contains(&format!("sink = \"{name}\"")), "{snapshot}");
        assert!(snapshot.contains("[jobs]"), "{snapshot}");
    }
}
