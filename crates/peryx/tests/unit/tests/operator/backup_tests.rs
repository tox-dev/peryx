use peryx_storage::blob::BlobStore;
use peryx_storage::meta::{ArtifactSource, MetaStore};

use std::path::PathBuf;

use rstest::rstest;

use crate::config::{
    self, AvailabilityConfig, BlobStorageConfig, Config, DcMember, DcMembership, DcRole, IndexKind, JobsConfig,
    JobsMode, LogConfig, LogFormat, LogSink, ReplicationConfig, S3StorageConfig, SecretSource,
};
use crate::operator;

#[cfg(windows)]
const fn exec_path() -> &'static str {
    r"C:\credential-helper.exe"
}

#[cfg(not(windows))]
const fn exec_path() -> &'static str {
    "/credential-helper"
}

use super::{FailOnLine, backup_fixture};

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
fn test_backup_create_rejects_live_metadata_store() {
    let (_source, config, _content_digest, _metadata_digest) = backup_fixture();
    let live = MetaStore::open(config.data_dir.join("peryx.redb")).unwrap();
    let root = tempfile::tempdir().unwrap();
    let backup = root.path().join("backup");

    let err = operator::backup_create(&config, &backup, &mut Vec::new()).unwrap_err();

    assert!(err.to_string().contains("is open by a running node"));
    assert!(!backup.join("manifest.json").exists());
    drop(live);
}

#[test]
fn test_backup_create_succeeds_on_stopped_store() {
    let (_source, config, _content_digest, _metadata_digest) = backup_fixture();
    let root = tempfile::tempdir().unwrap();
    let backup = root.path().join("backup");

    operator::backup_create(&config, &backup, &mut Vec::new()).unwrap();

    assert!(backup.join("manifest.json").exists());
}

#[test]
fn test_backup_create_reports_unreadable_source() {
    let (_source, config, _content_digest, _metadata_digest) = backup_fixture();
    std::fs::remove_file(config.data_dir.join("peryx.redb")).unwrap();
    let root = tempfile::tempdir().unwrap();
    let backup = root.path().join("backup");

    let err = operator::backup_create(&config, &backup, &mut Vec::new()).unwrap_err();

    assert!(err.to_string().contains("read-only"));
    assert!(!backup.exists());
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
#[case::dc_primary(
    "[tls]\ncert = \"/etc/peryx/tls.crt\"\nkey = \"/etc/peryx/tls.key\"",
    "[availability]\nmode = \"dc\"\n[availability.replication]\nrole = \"primary\"\nsource = \"primary-a\"\ntoken = \"replication-token\""
)]
#[case::ha_replica(
    "[acme]\ndomains = [\"packages.example.com\"]\ncontact = \"ops@example.com\"\ncache-dir = \"/var/cache/peryx/acme\"\nstaging = true",
    "[availability]\nmode = \"ha\"\n[availability.replication]\nrole = \"replica\"\nupstream = \"https://primary.example/\"\ntoken_file = \"/run/secrets/replication-token\"\npoll_interval_secs = 30\npage_size = 250"
)]
fn test_backup_config_round_trips_effective_settings(#[case] tls: &str, #[case] availability: &str) {
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
repository = "root/pypi"

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
scopes = ["openid", "email", "groups"]
subject_claim = "sub"
display_name_claim = "name"
groups_claim = "groups"
clock_skew_secs = 45
request_timeout_secs = 8

[[auth.oidc_provider.group_mapping]]
group = "registry-admins"
role = "administrator"

[[index]]
name = "python"
route = "root/python"
ecosystem = "pypi"
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

[[index.upstream]]
name = "primary"
url = "https://pypi.example/simple/"
username = "mirror"
password = "mirror-secret"

[[index]]
name = "hub"
ecosystem = "oci"
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

[[index.upstream]]
name = "primary"
url = "https://registry-1.docker.io"
token_env = "DOCKERHUB_TOKEN"
credential_refresh_secs = 60

[[index]]
name = "images"
ecosystem = "oci"
hosted = true
volatile = false
anonymous_read = false

[index.policy]
allow_projects = []
block_projects = ["internal/*"]

[[index.access_token]]
name = "uploader"
secret_file = "/run/secrets/upload-token"
actions = ["write", "delete"]

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
username = "mirror"
password_file = "/run/secrets/internal-password"
credential_refresh_secs = 45
credential_refresh_on_unauthorized = false
credential_failure = "anonymous"
ca_file = "/run/secrets/internal-ca.pem"
client_cert_file = "/run/secrets/internal-client.pem"
client_key_file = "/run/secrets/internal-client-key.pem"

[[index.upstream]]
name = "public"
url = "https://pypi.org/simple/"

[index.upstream.credential_exec]
argv = [{:?}, "--profile", "public"]
timeout_secs = 12
environment = ["HOME", "AWS_PROFILE"]
failure = "anonymous"

[[index.upstream]]
name = "backup"
url = "https://backup.example/simple/"

[index.upstream.credential_exec]
argv = [{:?}]
"#,
        data_dir.display().to_string(),
        exec_path(),
        exec_path()
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
fn test_backup_config_round_trips_exec_credentials() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = root.path().join("data");
    std::fs::create_dir(&data_dir).unwrap();
    drop(MetaStore::open(data_dir.join("peryx.redb")).unwrap());
    let source = format!(
        "data_dir = {:?}\n[[index]]\nname = \"pypi\"\n\
         [[index.upstream]]\nname = \"primary\"\nurl = \"https://pypi.org/simple/\"\n\
         [index.upstream.credential_exec]\nargv = [{:?}, \"--profile\", \"public\"]\ntimeout_secs = 12\n\
         environment = [\"HOME\", \"AWS_PROFILE\"]\nfailure = \"anonymous\"\n",
        data_dir.display().to_string(),
        exec_path()
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
fn test_backup_rejects_the_s3_blob_backend_before_writing_anything() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = root.path().join("data");
    std::fs::create_dir(&data_dir).unwrap();
    drop(MetaStore::open(data_dir.join("peryx.redb")).unwrap());
    let config = Config {
        data_dir,
        blob: crate::tests::s3_blob_backend(),
        ..Config::default()
    };
    let backup = root.path().join("backup");

    let error = operator::backup_create(&config, &backup, &mut Vec::new()).unwrap_err();

    let message = error.to_string();
    assert!(message.contains("S3"), "{message}");
    assert!(message.contains("filesystem-backed repository"), "{message}");
    assert!(!backup.exists());
}

#[test]
fn test_backup_rejects_a_secret_bearing_s3_endpoint_without_leaking_it() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = root.path().join("data");
    std::fs::create_dir(&data_dir).unwrap();
    drop(MetaStore::open(data_dir.join("peryx.redb")).unwrap());
    let secret = "endpoint-password";
    let config = Config {
        data_dir,
        blob: BlobStorageConfig::S3(S3StorageConfig {
            endpoint: format!("https://user:{secret}@s3.example.com"),
            bucket: "cache".to_owned(),
            prefix: String::new(),
            region: "us-east-1".to_owned(),
            path_style: false,
            request_timeout: std::time::Duration::from_secs(30),
            max_retries: 3,
            multipart_threshold: 16 << 20,
            part_size: 16 << 20,
            upload_concurrency: 4,
            conditional_writes: true,
            checksum_writes: true,
        }),
        ..Config::default()
    };
    assert!(!format!("{config:?}").contains(secret));
    let error = operator::backup_create(&config, &root.path().join("backup"), &mut Vec::new()).unwrap_err();
    assert!(!format!("{error:?}").contains(secret));
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
    use peryx_driver::jobs::{CatalogSyncParameters, DcCopyParameters, Schedule, ScheduledJob};

    let root = tempfile::tempdir().unwrap();
    let data_dir = root.path().join("data");
    std::fs::create_dir(&data_dir).unwrap();
    drop(MetaStore::open(data_dir.join("peryx.redb")).unwrap());
    let backup = root.path().join("backup");

    let schedules = vec![
        Schedule {
            job: ScheduledJob::CacheMaintenance,
            interval: std::time::Duration::from_mins(5),
        },
        Schedule {
            job: ScheduledJob::CatalogSync(CatalogSyncParameters {
                repository: "pypi".to_owned(),
                source: None,
                max_projects: std::num::NonZeroUsize::new(9).unwrap(),
                concurrency: std::num::NonZeroUsize::new(2).unwrap(),
                timeout: std::time::Duration::from_secs(30),
            }),
            interval: std::time::Duration::from_hours(1),
        },
        Schedule {
            job: ScheduledJob::DcCopy(DcCopyParameters {
                concurrency: std::num::NonZeroUsize::new(4).unwrap(),
            }),
            interval: std::time::Duration::from_mins(2),
        },
    ];
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
    assert!(snapshot.contains("max_projects = 9"), "{snapshot}");

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
    let IndexKind::Cached { routing, .. } = &mut config.indexes[0].kind else {
        panic!("expected a cached index");
    };
    let primary = &mut routing.upstreams[0];
    primary.password = Some(SecretSource::File(PathBuf::from("/run/secrets/pw")));
    primary.token = Some(SecretSource::File(PathBuf::from("/run/secrets/tok")));
    primary.tls.ca_file = Some(PathBuf::from("/run/secrets/ca.pem"));
    primary.tls.client_cert_file = Some(PathBuf::from("/run/secrets/client.pem"));
    primary.tls.client_key_file = Some(PathBuf::from("/run/secrets/client-key.pem"));

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
    let IndexKind::Cached { routing, .. } = &mut config.indexes[0].kind else {
        panic!("expected a cached index");
    };
    routing.upstreams[0].password = Some(SecretSource::Env("CORP_PASSWORD".to_owned()));
    routing.upstreams[0].token = Some(SecretSource::Env("CORP_TOKEN".to_owned()));

    operator::backup_create(&config, &backup, &mut Vec::new()).unwrap();

    let snapshot = std::fs::read_to_string(backup.join("config.toml")).unwrap();
    assert!(snapshot.contains("password_env = \"CORP_PASSWORD\""), "{snapshot}");
    assert!(snapshot.contains("token_env = \"CORP_TOKEN\""), "{snapshot}");
}

fn read_manifest_json(backup: &std::path::Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(backup.join("manifest.json")).unwrap()).unwrap()
}

#[test]
fn test_backup_records_dc_availability_state_and_membership() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = root.path().join("data");
    std::fs::create_dir(&data_dir).unwrap();
    let meta = MetaStore::open(data_dir.join("peryx.redb")).unwrap();
    meta.record_artifact_placement("sha256:aa", ArtifactSource::Hosted, true)
        .unwrap();
    meta.record_artifact_placement("sha256:bb", ArtifactSource::Proxy, false)
        .unwrap();
    let frontier = meta.current_serial().unwrap();
    drop(meta);

    let config = Config {
        data_dir,
        availability: AvailabilityConfig::Dc(ReplicationConfig::Primary {
            source: "primary-a".to_owned(),
            token: SecretSource::Literal("replication-token".to_owned()),
        }),
        dc_membership: Some(DcMembership {
            group: "group-a".to_owned(),
            members: vec![
                DcMember {
                    node: "node-a".to_owned(),
                    dc: "east".to_owned(),
                    address: "10.0.0.1:8443".to_owned(),
                    role: DcRole::Writer,
                },
                DcMember {
                    node: "node-b".to_owned(),
                    dc: "west".to_owned(),
                    address: "10.0.0.2:8443".to_owned(),
                    role: DcRole::Replica,
                },
            ],
        }),
        ..Config::default()
    };
    let backup = root.path().join("backup");
    let mut out = Vec::new();
    operator::backup_create(&config, &backup, &mut out).unwrap();

    assert!(String::from_utf8(out).unwrap().contains("availability\tdc\tfrontier"));
    let manifest = read_manifest_json(&backup);
    let availability = &manifest["availability"];
    assert_eq!(availability["mode"], "dc");
    assert_eq!(availability["metadata_frontier"], frontier);
    assert_eq!(availability["placements"], 2);
    assert_eq!(availability["membership"]["group"], "group-a");
    assert_eq!(availability["membership"]["members"][0]["node"], "node-a");
    assert_eq!(availability["membership"]["members"][0]["dc"], "east");
    assert_eq!(availability["membership"]["members"][0]["address"], "10.0.0.1:8443");
    assert_eq!(availability["membership"]["members"][0]["role"], "writer");
    assert_eq!(availability["membership"]["members"][1]["role"], "replica");

    operator::backup_verify(&backup, &mut Vec::new()).unwrap();
}

#[test]
fn test_backup_create_propagates_availability_summary_write_error() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = root.path().join("data");
    std::fs::create_dir(&data_dir).unwrap();
    let meta = MetaStore::open(data_dir.join("peryx.redb")).unwrap();
    meta.record_artifact_placement("sha256:aa", ArtifactSource::Hosted, true)
        .unwrap();
    let frontier = meta.current_serial().unwrap();
    drop(meta);
    let config = Config {
        data_dir,
        availability: AvailabilityConfig::Dc(ReplicationConfig::Primary {
            source: "primary-a".to_owned(),
            token: SecretSource::Literal("replication-token".to_owned()),
        }),
        ..Config::default()
    };
    let backup = root.path().join("backup");

    let mut out = FailOnLine {
        needle: "\tplacements",
        ..FailOnLine::default()
    };
    operator::backup_create(&config, &backup, &mut out).unwrap_err();

    assert!(out.seen.contains("blobs\t"), "{}", out.seen);
    assert!(
        out.seen.contains(&format!("availability\tdc\tfrontier {frontier}")),
        "{}",
        out.seen
    );
}

#[test]
fn test_backup_records_none_availability_without_membership() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = root.path().join("data");
    std::fs::create_dir(&data_dir).unwrap();
    drop(MetaStore::open(data_dir.join("peryx.redb")).unwrap());
    let config = Config {
        data_dir,
        ..Config::default()
    };
    let backup = root.path().join("backup");
    operator::backup_create(&config, &backup, &mut Vec::new()).unwrap();

    let manifest = read_manifest_json(&backup);
    let availability = &manifest["availability"];
    assert_eq!(availability["mode"], "none");
    assert_eq!(availability["placements"], 0);
    assert!(availability.get("membership").is_none());
    assert!(availability.get("writer_identity").is_none());
}

#[test]
fn test_backup_records_the_writer_identity() {
    let (_holder, backup) = super::identified_backup("node-a", 1);

    let manifest = read_manifest_json(&backup);

    assert_eq!(manifest["availability"]["writer_identity"], "node-a");
}
