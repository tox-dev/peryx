use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;

use std::path::PathBuf;

use rstest::rstest;

use crate::config::{
    self, BlobStorageConfig, Config, IndexKind, JobsConfig, JobsMode, LogConfig, LogFormat, LogSink, S3StorageConfig,
    SecretSource,
};
use crate::operator;

use super::backup_fixture;

#[test]
fn test_backup_create_rejects_existing_target_paths() {
    let (_source, config, _content_digest, _metadata_digest) = backup_fixture();
    let root = tempfile::tempdir().unwrap();
    let file_target = root.path().join("file-backup");
    std::fs::write(&file_target, b"x").unwrap();

    let err = operator::backup_create(&config, &file_target, &mut Vec::new()).unwrap_err();
    assert!(err.to_string().contains("exists and is not a directory"));

    let dir_target = root.path().join("dir-backup");
    std::fs::create_dir(&dir_target).unwrap();
    std::fs::write(dir_target.join("blocker"), b"x").unwrap();
    let err = operator::backup_create(&config, &dir_target, &mut Vec::new()).unwrap_err();
    assert!(err.to_string().contains("is not empty"));
}

#[test]
fn test_backup_create_rejects_missing_source_blob() {
    let (_source, config, content_digest, _metadata_digest) = backup_fixture();
    std::fs::remove_file(BlobStore::new(config.data_dir.join("blobs")).path_for(&content_digest)).unwrap();
    let root = tempfile::tempdir().unwrap();

    let err = operator::backup_create(&config, &root.path().join("backup"), &mut Vec::new()).unwrap_err();

    assert!(err.to_string().contains("referenced blob"));
    assert!(err.to_string().contains("is missing"));
}

#[test]
fn test_backup_create_rejects_tampered_source_blob() {
    let (_source, config, content_digest, _metadata_digest) = backup_fixture();
    std::fs::write(
        BlobStore::new(config.data_dir.join("blobs")).path_for(&content_digest),
        b"tampered",
    )
    .unwrap();
    let root = tempfile::tempdir().unwrap();

    let err = operator::backup_create(&config, &root.path().join("backup"), &mut Vec::new()).unwrap_err();

    assert!(err.to_string().contains("hashed as"));
}

#[rstest]
#[case::manual_primary(
    "[tls]\ncert = \"/etc/peryx/tls.crt\"\nkey = \"/etc/peryx/tls.key\"",
    "[replication]\nrole = \"primary\"\nsource = \"primary-a\"\ntoken = \"replication-token\""
)]
#[case::acme_replica(
    "[acme]\ndomains = [\"packages.example.com\"]\ncontact = \"ops@example.com\"\ncache-dir = \"/var/cache/peryx/acme\"\nstaging = true",
    "[replication]\nrole = \"replica\"\nupstream = \"https://primary.example/\"\ntoken_file = \"/run/secrets/replication-token\"\npoll_interval_secs = 30\npage_size = 250"
)]
fn test_backup_config_round_trips_effective_settings(#[case] tls: &str, #[case] replication: &str) {
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

{replication}

[log]
level = "peryx=debug"
format = "json"
sink = "file"
file = "/var/log/peryx.log"

[rate_limit]
enabled = true
max_clients = 17
trusted_proxies = ["127.0.0.1/32", "2001:db8::/32"]

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

[auth]
signing_key_file = "/run/secrets/signing-key"
token_ttl_secs = 601
default_anonymous_read = false
oidc_audience = "https://packages.example/_/oidc"

[[auth.trusted_publisher]]
id = "release"
issuer = "https://token.actions.githubusercontent.com"
repository = "root/pypi"
subject = "repo:tox-dev/peryx:*"
projects = ["peryx"]

[auth.trusted_publisher.claims]
repository_id = "123456789"

[[index]]
name = "python"
route = "root/python"
ecosystem = "pypi"
cached = "https://pypi.example/simple/"
username = "mirror"
password = "mirror-secret"
upstream_concurrency = 7
offline = true
anonymous_read = true

[index.prefetch]
mode = "all"
packages = ["flask"]
requirements = ["requirements.txt"]
include_wheels = false
include_sdists = true
python_tags = ["cp313"]
abi_tags = ["cp313"]
platform_tags = ["manylinux_2_28_x86_64"]
max_file_size_bytes = 9001
metadata_only = false

[index.policy]
allow_projects = ["flask"]
block_projects = ["blocked"]
max_file_size_bytes = 8001
max_project_size_bytes = 8002
max_accounted_bytes = 8003
max_projects = 12
max_versions_per_project = 34
quota_audit = true
allow_versions = ">=1"

[[index]]
name = "hub"
ecosystem = "oci"
cached = "https://registry-1.docker.io"
token = "upstream-token"
upstream_concurrency = 9
offline = false

[index.prefetch]
mode = "metadata-only"
packages = ["library/nginx"]
requirements = []
include_wheels = true
include_sdists = false
python_tags = []
abi_tags = []
platform_tags = []
metadata_only = true

[index.policy]
allow_projects = ["library/*"]
block_projects = []

[index.settings]
library_prefix = true

[[index]]
name = "images"
ecosystem = "oci"
hosted = true
upload_token_file = "/run/secrets/upload-token"
volatile = false
anonymous_read = false

[index.policy]
allow_projects = []
block_projects = ["internal/*"]

[[index.access_token]]
name = "ci"
secret_file = "/run/secrets/ci-token"
projects = ["team/*"]
actions = ["read", "write"]
expires_at = "2027-01-01T00:00:00Z"

[[index.access_token]]
name = "janitor"
secret = "janitor-secret"
projects = ["*"]
actions = ["delete"]

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
name = "root/oci"
ecosystem = "oci"
layers = ["images", "hub"]
upload = "images"

[index.policy]
allow_projects = []
block_projects = []
"#
    );
    let config = Config::default()
        .apply(config::from_toml(PathBuf::from("source.toml"), &source).unwrap())
        .unwrap();
    let backup = root.path().join("backup");
    operator::backup_create(&config, &backup, &mut Vec::new()).unwrap();

    let snapshot = std::fs::read_to_string(backup.join("config.toml")).unwrap();
    let restored = Config::default()
        .apply(config::from_toml(PathBuf::from("config.toml"), &snapshot).unwrap())
        .unwrap();

    assert_eq!(restored, config);
}

#[test]
fn test_backup_create_snapshots_log_variants() {
    for (format, sink, expected) in [
        (LogFormat::Json, LogSink::File, "format = \"json\"\nsink = \"file\""),
        (
            LogFormat::Pretty,
            LogSink::Journald,
            "format = \"pretty\"\nsink = \"journald\"",
        ),
        (
            LogFormat::Pretty,
            LogSink::Syslog,
            "format = \"pretty\"\nsink = \"syslog\"",
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let data_dir = root.path().join("data");
        std::fs::create_dir(&data_dir).unwrap();
        drop(MetaStore::open(data_dir.join("peryx.redb")).unwrap());
        let backup = root.path().join("backup");

        operator::backup_create(
            &Config {
                data_dir,
                log: LogConfig {
                    format,
                    sink,
                    file: Some(root.path().join("peryx.log")),
                    ..LogConfig::default()
                },
                ..Config::default()
            },
            &backup,
            &mut Vec::new(),
        )
        .unwrap();

        assert!(
            std::fs::read_to_string(backup.join("config.toml"))
                .unwrap()
                .contains(expected)
        );
    }
}

#[test]
fn test_backup_config_round_trips_upstream_routes() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = root.path().join("data");
    std::fs::create_dir(&data_dir).unwrap();
    drop(MetaStore::open(data_dir.join("peryx.redb")).unwrap());
    let source = format!(
        r#"
data_dir = {:?}

[[index]]
name = "pypi"
fallback = false
protected = ["internal-pkg"]

[index.pins]
flask = "public"

[[index.upstream]]
name = "internal"
url = "https://packages.example/simple/"
artifact_url = "https://artifacts.example/"
password_file = "/run/secrets/internal-password"
ca_file = "/run/secrets/internal-ca.pem"
client_cert_file = "/run/secrets/internal-client.pem"
client_key_file = "/run/secrets/internal-client-key.pem"

[[index.upstream]]
name = "public"
url = "https://pypi.org/simple/"
token = "bearer"
"#,
        data_dir.display().to_string()
    );
    let config = Config::default()
        .apply(config::from_toml(PathBuf::from("source.toml"), &source).unwrap())
        .unwrap();
    let backup = root.path().join("backup");

    operator::backup_create(&config, &backup, &mut Vec::new()).unwrap();
    let snapshot = std::fs::read_to_string(backup.join("config.toml")).unwrap();
    let restored = Config::default()
        .apply(config::from_toml(PathBuf::from("config.toml"), &snapshot).unwrap())
        .unwrap();

    assert_eq!(restored, config);
}

#[rstest]
#[case::literal(SecretSource::Literal("s3cret".to_owned()), "upload_token = \"s3cret\"")]
#[case::file(
    SecretSource::File(PathBuf::from("/run/secrets/token")),
    "upload_token_file = \"/run/secrets/token\""
)]
fn test_backup_snapshots_where_the_upload_token_lives(#[case] source: SecretSource, #[case] expected: &str) {
    let root = tempfile::tempdir().unwrap();
    let data_dir = root.path().join("data");
    std::fs::create_dir(&data_dir).unwrap();
    drop(MetaStore::open(data_dir.join("peryx.redb")).unwrap());
    let backup = root.path().join("backup");
    let mut config = Config {
        data_dir,
        ..Config::default()
    };
    config.indexes[1].kind = IndexKind::Hosted {
        upload_token: Some(source),
        volatile: true,
    };

    operator::backup_create(&config, &backup, &mut Vec::new()).unwrap();

    let snapshot = std::fs::read_to_string(backup.join("config.toml")).unwrap();
    assert!(snapshot.contains(expected), "{snapshot}");
}

#[test]
fn test_backup_snapshots_the_s3_blob_backend_and_restores_it() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = root.path().join("data");
    std::fs::create_dir(&data_dir).unwrap();
    drop(MetaStore::open(data_dir.join("peryx.redb")).unwrap());
    let s3 = S3StorageConfig {
        endpoint: "https://s3.example.com".to_owned(),
        bucket: "cache".to_owned(),
        prefix: "peryx".to_owned(),
        region: "us-east-1".to_owned(),
        path_style: true,
        request_timeout: std::time::Duration::from_secs(20),
        max_retries: 4,
        multipart_threshold: 1024,
        part_size: 2048,
        upload_concurrency: 6,
    };
    let config = Config {
        data_dir,
        blob: BlobStorageConfig::S3(s3.clone()),
        ..Config::default()
    };
    let backup = root.path().join("backup");
    operator::backup_create(&config, &backup, &mut Vec::new()).unwrap();
    let snapshot = std::fs::read_to_string(backup.join("config.toml")).unwrap();
    assert!(snapshot.contains("backend = \"s3\""), "{snapshot}");
    assert!(!snapshot.to_lowercase().contains("secret"), "{snapshot}");
    let restored = Config::default()
        .apply(config::from_toml(PathBuf::from("config.toml"), &snapshot).unwrap())
        .unwrap();
    assert_eq!(restored.blob, BlobStorageConfig::S3(s3));
}

#[test]
fn test_backup_snapshots_disabled_jobs_but_omits_the_default() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = root.path().join("data");
    std::fs::create_dir(&data_dir).unwrap();
    drop(MetaStore::open(data_dir.join("peryx.redb")).unwrap());
    let backup = root.path().join("backup");

    let default = Config {
        data_dir,
        ..Config::default()
    };
    operator::backup_create(&default, &backup, &mut Vec::new()).unwrap();
    assert!(
        !std::fs::read_to_string(backup.join("config.toml"))
            .unwrap()
            .contains("[jobs]")
    );

    let disabled = Config {
        jobs: JobsConfig {
            mode: JobsMode::None,
            ..JobsConfig::default()
        },
        ..default
    };
    let backup = root.path().join("backup-none");
    operator::backup_create(&disabled, &backup, &mut Vec::new()).unwrap();
    let snapshot = std::fs::read_to_string(backup.join("config.toml")).unwrap();
    let restored = Config::default()
        .apply(config::from_toml(PathBuf::from("config.toml"), &snapshot).unwrap())
        .unwrap();
    assert_eq!(restored.jobs.mode, JobsMode::None);
}

#[test]
fn test_backup_roundtrips_custom_job_schedules() {
    use peryx_driver::jobs::{Schedule, ScheduledJob};

    let root = tempfile::tempdir().unwrap();
    let data_dir = root.path().join("data");
    std::fs::create_dir(&data_dir).unwrap();
    drop(MetaStore::open(data_dir.join("peryx.redb")).unwrap());
    let backup = root.path().join("backup");

    let schedules = vec![Schedule {
        job: ScheduledJob::CacheMaintenance,
        interval: std::time::Duration::from_mins(5),
    }];
    let config = Config {
        data_dir,
        jobs: JobsConfig {
            mode: JobsMode::Local,
            schedules: schedules.clone(),
        },
        ..Config::default()
    };
    operator::backup_create(&config, &backup, &mut Vec::new()).unwrap();
    let snapshot = std::fs::read_to_string(backup.join("config.toml")).unwrap();
    assert!(snapshot.contains("[[jobs.schedule]]"), "{snapshot}");

    let restored = Config::default()
        .apply(config::from_toml(PathBuf::from("config.toml"), &snapshot).unwrap())
        .unwrap();
    assert_eq!(restored.jobs.schedules, schedules);
}

#[rstest]
#[case::password("password_file = \"/run/secrets/pw\"")]
#[case::token("token_file = \"/run/secrets/tok\"")]
#[case::ca("ca_file = \"/run/secrets/ca.pem\"")]
#[case::certificate("client_cert_file = \"/run/secrets/client.pem\"")]
#[case::key("client_key_file = \"/run/secrets/client-key.pem\"")]
fn test_backup_snapshots_upstream_secret_paths(#[case] expected: &str) {
    let root = tempfile::tempdir().unwrap();
    let data_dir = root.path().join("data");
    std::fs::create_dir(&data_dir).unwrap();
    drop(MetaStore::open(data_dir.join("peryx.redb")).unwrap());
    let backup = root.path().join("backup");
    let mut config = Config {
        data_dir,
        ..Config::default()
    };
    let IndexKind::Cached {
        password, token, tls, ..
    } = &mut config.indexes[0].kind
    else {
        panic!("expected a cached index");
    };
    *password = Some(SecretSource::File(PathBuf::from("/run/secrets/pw")));
    *token = Some(SecretSource::File(PathBuf::from("/run/secrets/tok")));
    tls.ca_file = Some(PathBuf::from("/run/secrets/ca.pem"));
    tls.client_cert_file = Some(PathBuf::from("/run/secrets/client.pem"));
    tls.client_key_file = Some(PathBuf::from("/run/secrets/client-key.pem"));

    operator::backup_create(&config, &backup, &mut Vec::new()).unwrap();

    let snapshot = std::fs::read_to_string(backup.join("config.toml")).unwrap();
    assert!(snapshot.contains(expected), "{snapshot}");
}

#[test]
fn test_backup_snapshots_upstream_env_references_not_values() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = root.path().join("data");
    std::fs::create_dir(&data_dir).unwrap();
    drop(MetaStore::open(data_dir.join("peryx.redb")).unwrap());
    let backup = root.path().join("backup");
    let mut config = Config {
        data_dir,
        ..Config::default()
    };
    let IndexKind::Cached { password, token, .. } = &mut config.indexes[0].kind else {
        panic!("expected a cached index");
    };
    *password = Some(SecretSource::Env("CORP_PASSWORD".to_owned()));
    *token = Some(SecretSource::Env("CORP_TOKEN".to_owned()));

    operator::backup_create(&config, &backup, &mut Vec::new()).unwrap();

    let snapshot = std::fs::read_to_string(backup.join("config.toml")).unwrap();
    assert!(snapshot.contains("password_env = \"CORP_PASSWORD\""), "{snapshot}");
    assert!(snapshot.contains("token_env = \"CORP_TOKEN\""), "{snapshot}");
}
