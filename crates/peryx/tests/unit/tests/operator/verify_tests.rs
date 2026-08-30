use std::fmt::Write as _;

use peryx_storage::meta::MetaStore;
use rstest::rstest;

use crate::config::{self, Config};
use crate::operator;

use super::support::{
    BackupFixture, backup_verify, blob_relpath, identified_backup, mutate_manifest, resign_file, valid_backup,
};
use crate::tests::support::{plugins_with_broken_blob_references, store_repositories};

fn backup_with_uncovered_stored_ecosystem() -> (tempfile::TempDir, std::path::PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let data_dir = root.path().join("data");
    std::fs::create_dir(&data_dir).unwrap();
    let meta = MetaStore::open(data_dir.join("peryx.redb")).unwrap();
    store_repositories(&meta, &["pypi", "oci"]);
    drop(meta);
    let backup = root.path().join("backup");
    operator::backup_create(
        &Config {
            data_dir,
            ..Config::default()
        },
        &backup,
        &mut Vec::new(),
    )
    .unwrap();
    let config_path = backup.join("config.toml");
    let mut snapshot = toml::from_str::<toml::Value>(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    snapshot["index"]
        .as_array_mut()
        .unwrap()
        .retain(|index| index["ecosystem"].as_str() == Some("pypi"));
    std::fs::write(&config_path, toml::to_string_pretty(&snapshot).unwrap()).unwrap();
    resign_file(&backup, "config", "config.toml");
    (root, backup)
}

#[test]
fn test_verify_accepts_a_complete_backup_without_mutation() {
    let fixture = valid_backup();
    let metadata = fixture.backup.join("metadata/peryx.redb");
    let before = std::fs::read(&metadata).unwrap();
    let mut out = Vec::new();

    backup_verify(&fixture.backup, &mut out).unwrap();

    assert_eq!(
        (out, std::fs::read(metadata).unwrap()),
        (b"scope\tecosystems\tcore\nok\n".to_vec(), before)
    );
}

#[test]
fn test_verify_reports_an_uncovered_stored_ecosystem() {
    let (_root, backup) = backup_with_uncovered_stored_ecosystem();
    let mut output = Vec::new();

    let error = operator::backup_verify(&backup, &mut output).unwrap_err();

    assert_eq!(
        (error.to_string(), String::from_utf8(output).unwrap()),
        (
            "backup verification failed with 1 problem(s)".to_owned(),
            concat!(
                "problem\tmetadata-reference-scope\toci\tmissing blob-reference driver\n",
                "problems\t1\n"
            )
            .to_owned(),
        )
    );
}

#[test]
fn test_verify_propagates_uncovered_scope_output_errors() {
    let (_root, backup) = backup_with_uncovered_stored_ecosystem();
    let mut output: &mut [u8] = &mut [];

    let error = operator::backup_verify(&backup, &mut output).unwrap_err();

    assert_eq!(
        error.downcast_ref::<std::io::Error>().map(std::io::Error::kind),
        Some(std::io::ErrorKind::WriteZero)
    );
}

#[test]
fn test_verify_propagates_blob_reference_driver_errors() {
    let fixture = valid_backup();

    let error =
        operator::backup_verify_with_plugins(&fixture.backup, &plugins_with_broken_blob_references(), &mut Vec::new())
            .unwrap_err();

    assert_eq!(
        format!("{error:#}"),
        "scan backup metadata blob references: scan core blob references: blob-reference scan failed"
    );
}

#[rstest]
#[case::missing_ancestor(BlobFailure::MissingAncestor, "missing")]
#[case::missing_file(BlobFailure::MissingFile, "missing")]
#[case::non_directory_ancestor(BlobFailure::NonDirectoryAncestor, "missing")]
#[case::mismatched(BlobFailure::Mismatched, "sha256 expected")]
fn test_verify_reports_blob_failures(#[case] failure: BlobFailure, #[case] expected: &str) {
    let fixture = valid_backup();
    let blob = fixture.backup.join(blob_relpath(&fixture.content_digest));
    match failure {
        BlobFailure::MissingAncestor => std::fs::remove_dir_all(blob.parent().unwrap()).unwrap(),
        BlobFailure::MissingFile => std::fs::remove_file(blob).unwrap(),
        BlobFailure::NonDirectoryAncestor => {
            std::fs::remove_dir_all(fixture.backup.join("blobs")).unwrap();
            std::fs::write(fixture.backup.join("blobs"), b"not a directory").unwrap();
        }
        BlobFailure::Mismatched => std::fs::write(blob, b"tampered").unwrap(),
    }
    let mut out = Vec::new();

    let error = backup_verify(&fixture.backup, &mut out).unwrap_err();

    assert_eq!(
        (
            error.to_string().contains("backup verification failed"),
            String::from_utf8(out).unwrap().contains(expected),
        ),
        (true, true)
    );
}

#[derive(Clone, Copy)]
enum BlobFailure {
    MissingAncestor,
    MissingFile,
    NonDirectoryAncestor,
    Mismatched,
}

#[test]
fn test_verify_rejects_an_unsupported_manifest() {
    let fixture = valid_backup();
    mutate_manifest(&fixture.backup, |manifest| {
        manifest["format"] = serde_json::json!(3);
    });

    let error = operator::backup_verify(&fixture.backup, &mut Vec::new()).unwrap_err();

    assert_eq!(error.to_string(), "unsupported backup format 3");
}

#[rstest]
#[case::config_parent("config", "config.toml", ManifestPath::Parent)]
#[case::metadata_parent("metadata", "metadata/peryx.redb", ManifestPath::Parent)]
#[case::blob_index_parent("blob_index", "blobs.tsv", ManifestPath::Parent)]
#[case::config_absolute("config", "config.toml", ManifestPath::Absolute)]
#[case::metadata_absolute("metadata", "metadata/peryx.redb", ManifestPath::Absolute)]
#[case::blob_index_absolute("blob_index", "blobs.tsv", ManifestPath::Absolute)]
#[case::empty("config", "config.toml", ManifestPath::Empty)]
#[case::current("config", "config.toml", ManifestPath::Current)]
#[case::prefix("config", "config.toml", ManifestPath::Prefix)]
fn test_verify_rejects_invalid_manifest_paths(#[case] field: &str, #[case] expected: &str, #[case] path: ManifestPath) {
    let fixture = valid_backup();
    let external = fixture.root.path().join("outside");
    std::fs::write(&external, b"external").unwrap();
    let invalid = match path {
        ManifestPath::Absolute => external.to_string_lossy().into_owned(),
        ManifestPath::Current => format!("./{expected}"),
        ManifestPath::Empty => String::new(),
        ManifestPath::Parent => "../outside".to_owned(),
        ManifestPath::Prefix => r"C:\outside".to_owned(),
    };
    mutate_manifest(&fixture.backup, |manifest| {
        manifest[field]["path"] = serde_json::json!(&invalid);
    });

    let error = backup_verify(&fixture.backup, &mut Vec::new()).unwrap_err();

    assert_eq!(
        (error.to_string(), std::fs::read(external).unwrap()),
        (
            format!(
                "invalid {} path {invalid:?}; expected {expected:?}",
                field.replace('_', " ")
            ),
            b"external".to_vec(),
        )
    );
}

#[derive(Clone, Copy)]
enum ManifestPath {
    Absolute,
    Current,
    Empty,
    Parent,
    Prefix,
}

#[cfg(unix)]
#[test]
fn test_verify_rejects_a_symlinked_file() {
    let fixture = valid_backup();
    let external = fixture.root.path().join("outside-config");
    let expected = std::fs::read(fixture.backup.join("config.toml")).unwrap();
    std::fs::rename(fixture.backup.join("config.toml"), &external).unwrap();
    std::os::unix::fs::symlink(&external, fixture.backup.join("config.toml")).unwrap();

    let error = backup_verify(&fixture.backup, &mut Vec::new()).unwrap_err();

    assert_eq!(
        (
            error.to_string().contains("contains a symbolic link"),
            std::fs::read(external).unwrap(),
        ),
        (true, expected)
    );
}

#[cfg(unix)]
#[test]
fn test_verify_rejects_a_symlinked_ancestor() {
    let fixture = valid_backup();
    let external = fixture.root.path().join("outside-blobs");
    let blob = blob_relpath(&fixture.content_digest);
    std::fs::rename(fixture.backup.join("blobs"), &external).unwrap();
    std::os::unix::fs::symlink(&external, fixture.backup.join("blobs")).unwrap();

    let error = backup_verify(&fixture.backup, &mut Vec::new()).unwrap_err();

    assert_eq!(
        (
            error.to_string().contains("contains a symbolic link"),
            std::fs::read(external.join(blob.strip_prefix("blobs/").unwrap())).unwrap(),
        ),
        (true, b"artifact bytes".to_vec())
    );
}

#[rstest]
#[case::config("config.toml", "problem\tconfig\tconfig.toml\tmissing")]
#[case::index("blobs.tsv", "problem\tblob-index\tblobs.tsv\tmissing")]
#[case::metadata("metadata/peryx.redb", "problem\tmetadata\tmetadata/peryx.redb\tmissing")]
fn test_verify_reports_missing_manifest_files(#[case] relative: &str, #[case] expected: &str) {
    let fixture = valid_backup();
    std::fs::remove_file(fixture.backup.join(relative)).unwrap();
    let mut out = Vec::new();

    backup_verify(&fixture.backup, &mut out).unwrap_err();

    assert!(String::from_utf8(out).unwrap().contains(expected));
}

#[test]
fn test_verify_reports_a_non_file_metadata_store() {
    let fixture = valid_backup();
    let metadata = fixture.backup.join("metadata/peryx.redb");
    std::fs::remove_file(&metadata).unwrap();
    std::fs::create_dir(metadata).unwrap();
    let mut out = Vec::new();

    backup_verify(&fixture.backup, &mut out).unwrap_err();

    assert_eq!(out, b"problem\tmetadata\tmetadata/peryx.redb\tmissing\nproblems\t1\n");
}

#[test]
fn test_verify_reports_a_corrupt_metadata_store() {
    let fixture = valid_backup();
    std::fs::write(fixture.backup.join("metadata/peryx.redb"), b"not a redb database").unwrap();
    resign_file(&fixture.backup, "metadata", "metadata/peryx.redb");
    let mut out = Vec::new();

    backup_verify(&fixture.backup, &mut out).unwrap_err();

    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("problem\tmetadata\tmetadata/peryx.redb")
    );
}

#[cfg(unix)]
#[test]
fn test_verify_reports_an_unreadable_metadata_store() {
    use std::os::unix::fs::PermissionsExt as _;

    let fixture = valid_backup();
    let metadata = fixture.backup.join("metadata/peryx.redb");
    std::fs::set_permissions(&metadata, std::fs::Permissions::from_mode(0o000)).unwrap();
    let mut out = Vec::new();
    let result = backup_verify(&fixture.backup, &mut out);
    std::fs::set_permissions(metadata, std::fs::Permissions::from_mode(0o600)).unwrap();

    assert_eq!(
        (
            result.is_err(),
            String::from_utf8(out)
                .unwrap()
                .contains("problem\tmetadata\tmetadata/peryx.redb\tI/O error"),
        ),
        (true, true)
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn test_verify_reports_metadata_read_errors() {
    let backup_env = "PERYX_TEST_METADATA_READ_ERROR_BACKUP";
    if let Some(backup) = std::env::var_os(backup_env) {
        assert_metadata_read_error(std::path::Path::new(&backup));
        return;
    }
    let fixture = valid_backup();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "operator::tests::verify_tests::test_verify_reports_metadata_read_errors",
            "--nocapture",
        ])
        .env(backup_env, &fixture.backup)
        .output()
        .unwrap();

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn assert_metadata_read_error(backup: &std::path::Path) {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::fs::MetadataExt as _;
    use std::sync::mpsc;

    let metadata = backup.join("metadata/peryx.redb");
    std::fs::OpenOptions::new()
        .write(true)
        .open(&metadata)
        .unwrap()
        .set_len(1 << 30)
        .unwrap();
    let identity = std::fs::metadata(metadata).unwrap();
    let (ready_send, ready_receive) = mpsc::sync_channel(0);
    let watcher = std::thread::spawn(move || {
        #[cfg(target_os = "linux")]
        let descriptor_directory = "/proc/self/fd";
        #[cfg(target_os = "macos")]
        let descriptor_directory = "/dev/fd";
        let disposable = std::fs::File::open("/dev/null").unwrap();
        let disposable_entry = std::fs::read_dir(descriptor_directory)
            .unwrap()
            .map(Result::unwrap)
            .find(|entry| descriptor_number(entry) == disposable.as_raw_fd())
            .unwrap();
        drop(disposable);
        assert!(existing_descriptor(&disposable_entry).is_none());
        ready_send.send(()).unwrap();
        loop {
            for (descriptor, found) in std::fs::read_dir(descriptor_directory)
                .unwrap()
                .map(Result::unwrap)
                .filter_map(|entry| existing_descriptor(&entry))
            {
                if descriptor > 2
                    && found.ino() == identity.ino()
                    && (cfg!(target_os = "macos") || found.dev() == identity.dev())
                {
                    nix::unistd::close(descriptor).unwrap();
                    let replacement = std::fs::OpenOptions::new().write(true).open("/dev/null").unwrap();
                    assert_eq!(replacement.as_raw_fd(), descriptor);
                    return replacement;
                }
            }
        }
    });
    ready_receive.recv().unwrap();
    let mut out = Vec::new();

    let error = backup_verify(backup, &mut out).unwrap_err();

    let replacement = watcher.join().unwrap();
    std::mem::forget(replacement);
    assert_eq!(
        (error.to_string(), String::from_utf8(out).unwrap()),
        (
            "backup verification failed with 1 problem(s)".to_owned(),
            concat!(
                "problem\tmetadata\tmetadata/peryx.redb\tI/O error: Bad file descriptor (os error 9)\n",
                "problems\t1\n"
            )
            .to_owned(),
        )
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn existing_descriptor(entry: &std::fs::DirEntry) -> Option<(i32, std::fs::Metadata)> {
    let descriptor = descriptor_number(entry);
    match std::fs::metadata(entry.path()) {
        Ok(found) => Some((descriptor, found)),
        Err(error) => {
            #[cfg(target_os = "linux")]
            assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
            #[cfg(target_os = "macos")]
            assert_eq!(error.raw_os_error(), Some(nix::libc::EBADF));
            None
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn descriptor_number(entry: &std::fs::DirEntry) -> i32 {
    entry.file_name().to_string_lossy().parse().unwrap()
}

#[rstest]
#[case::sha("sha256", serde_json::json!("invalid"), "sha256 expected")]
#[case::size("size_bytes", serde_json::json!(0), "size expected 0")]
fn test_verify_reports_metadata_manifest_mismatches(
    #[case] field: &str,
    #[case] value: serde_json::Value,
    #[case] expected: &str,
) {
    let fixture = valid_backup();
    mutate_manifest(&fixture.backup, |manifest| {
        manifest["metadata"][field] = value;
    });
    let mut out = Vec::new();

    backup_verify(&fixture.backup, &mut out).unwrap_err();

    assert!(String::from_utf8(out).unwrap().contains(expected));
}

#[rstest]
#[case::sha("sha256", serde_json::json!("invalid"), "sha256 expected")]
#[case::size("size_bytes", serde_json::json!(0), "size expected 0")]
fn test_verify_reports_config_manifest_mismatches(
    #[case] field: &str,
    #[case] value: serde_json::Value,
    #[case] expected: &str,
) {
    let fixture = valid_backup();
    mutate_manifest(&fixture.backup, |manifest| {
        manifest["config"][field] = value;
    });
    let mut out = Vec::new();

    backup_verify(&fixture.backup, &mut out).unwrap_err();

    assert!(String::from_utf8(out).unwrap().contains(expected));
}

#[rstest]
#[case::header(BlobIndexMutation::Header, "invalid header")]
#[case::row(BlobIndexMutation::Row, "invalid row")]
#[case::digest(BlobIndexMutation::Digest, "invalid digest")]
#[case::size(BlobIndexMutation::Size, "invalid size")]
#[case::path(BlobIndexMutation::Path, "invalid path")]
#[case::duplicate(BlobIndexMutation::Duplicate, "duplicate digest")]
fn test_verify_reports_blob_index_row_errors(#[case] mutation: BlobIndexMutation, #[case] expected: &str) {
    let fixture = valid_backup();
    rewrite_blob_index(&fixture, mutated_blob_index(&fixture, mutation));
    let mut out = Vec::new();

    backup_verify(&fixture.backup, &mut out).unwrap_err();

    assert!(String::from_utf8(out).unwrap().contains(expected));
}

#[test]
fn test_verify_accepts_empty_blob_index_rows() {
    let fixture = valid_backup();
    rewrite_blob_index(&fixture, format!("{}\n", valid_blob_index(&fixture)));
    let mut out = Vec::new();

    backup_verify(&fixture.backup, &mut out).unwrap();

    assert_eq!(out, b"scope\tecosystems\tcore\nok\n");
}

#[test]
fn test_verify_does_not_follow_an_invalid_blob_path() {
    let fixture = valid_backup();
    let external = fixture.root.path().join("external-blob");
    std::fs::write(&external, b"artifact bytes").unwrap();
    rewrite_blob_index(
        &fixture,
        valid_blob_index(&fixture).replace(&blob_relpath(&fixture.content_digest), external.to_str().unwrap()),
    );
    std::fs::remove_file(fixture.backup.join(blob_relpath(&fixture.content_digest))).unwrap();
    let mut out = Vec::new();

    backup_verify(&fixture.backup, &mut out).unwrap_err();

    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("invalid path"));
    assert!(text.contains(&format!("problem\tblob\t{}\tmissing", fixture.content_digest.as_str())));
}

#[test]
fn test_verify_reports_a_missing_metadata_reference() {
    let fixture = valid_backup();
    rewrite_blob_index(
        &fixture,
        format!(
            "sha256\tsize_bytes\tpath\n{}\t11\t{}\n",
            fixture.content_digest.as_str(),
            blob_relpath(&fixture.content_digest)
        ),
    );
    mutate_manifest(&fixture.backup, |manifest| {
        manifest["blob_index"]["count"] = serde_json::json!(1);
        manifest["blob_index"]["blob_bytes"] = serde_json::json!(11);
    });
    let mut out = Vec::new();

    backup_verify(&fixture.backup, &mut out).unwrap_err();

    assert!(String::from_utf8(out).unwrap().contains(&format!(
        "{}\tmissing referenced digest",
        fixture.metadata_digest.as_str()
    )));
}

#[rstest]
#[case::count("count", serde_json::json!(999), "blob-index\tcount")]
#[case::bytes("blob_bytes", serde_json::json!(999), "blob-index\tbytes")]
fn test_verify_reports_blob_index_summary_mismatches(
    #[case] field: &str,
    #[case] value: serde_json::Value,
    #[case] expected: &str,
) {
    let fixture = valid_backup();
    mutate_manifest(&fixture.backup, |manifest| {
        manifest["blob_index"][field] = value;
    });
    let mut out = Vec::new();

    backup_verify(&fixture.backup, &mut out).unwrap_err();

    assert!(String::from_utf8(out).unwrap().contains(expected));
}

#[test]
fn test_verify_reports_an_indexed_blob_size_mismatch() {
    let fixture = valid_backup();
    rewrite_blob_index(
        &fixture,
        format!(
            "sha256\tsize_bytes\tpath\n{content}\t999\t{content_path}\n{metadata}\t14\t{metadata_path}\n",
            content = fixture.content_digest.as_str(),
            content_path = blob_relpath(&fixture.content_digest),
            metadata = fixture.metadata_digest.as_str(),
            metadata_path = blob_relpath(&fixture.metadata_digest),
        ),
    );
    mutate_manifest(&fixture.backup, |manifest| {
        manifest["blob_index"]["blob_bytes"] = serde_json::json!(1013);
    });
    let mut out = Vec::new();

    backup_verify(&fixture.backup, &mut out).unwrap_err();

    assert!(String::from_utf8(out).unwrap().contains("size expected 999"));
}

#[rstest]
#[case::frontier("frontier", "metadata_frontier", serde_json::json!(999))]
#[case::placements("placements", "placements", serde_json::json!(5))]
fn test_verify_rejects_mismatched_recovery_state(
    #[case] expected: &str,
    #[case] field: &str,
    #[case] value: serde_json::Value,
) {
    let fixture = valid_backup();
    mutate_manifest(&fixture.backup, |manifest| {
        manifest["availability"][field] = value;
    });
    let mut out = Vec::new();

    backup_verify(&fixture.backup, &mut out).unwrap_err();

    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains(&format!("problem\tavailability\t{expected}"))
    );
}

#[test]
fn test_verify_rejects_a_mismatched_writer_identity() {
    let (_holder, backup) = identified_backup("node-a", 1);
    mutate_manifest(&backup, |manifest| {
        manifest["availability"]["writer_identity"] = serde_json::json!("node-x");
    });
    let mut out = Vec::new();

    backup_verify(&backup, &mut out).unwrap_err();

    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("writer-identity\texpected node-x, found node-a")
    );
}

#[test]
fn test_verify_rejects_an_unsupported_availability_mode() {
    let fixture = valid_backup();
    mutate_manifest(&fixture.backup, |manifest| {
        manifest["availability"]["mode"] = serde_json::json!("unsupported");
    });
    let mut out = Vec::new();

    backup_verify(&fixture.backup, &mut out).unwrap_err();

    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("mode\tunsupported unsupported")
    );
}

type MembershipMember = (&'static str, &'static str, &'static str, &'static str);

fn membership_config(mode: &str, members: &[MembershipMember]) -> String {
    let listener = if mode == "ha" { "[availability.listener]\n" } else { "" };
    let roster = members.iter().fold(String::new(), |mut roster, (node, dc, address, role)| {
        write!(
            roster,
                "[[availability.member]]\nnode = \"{node}\"\ndc = \"{dc}\"\naddress = \"{address}\"\nrole = \"{role}\"\n"
        )
        .unwrap();
        roster
    });
    format!(
        "[availability]\nmode = \"{mode}\"\ngroup = \"g\"\n\
         [availability.replication]\nrole = \"primary\"\nsource = \"a\"\ntoken = \"t\"\n\
         {listener}{roster}"
    )
}

#[rstest]
#[case::dc_distinct("dc", &[("w", "east", "https://w:1", "writer"), ("r", "west", "https://r:1", "replica")], true)]
#[case::dc_shared("dc", &[("w", "east", "https://w:1", "writer"), ("r", "east", "https://r:1", "replica")], true)]
#[case::ha_distinct("ha", &[("w", "east", "https://w:1", "writer"), ("r", "west", "https://r:1", "replica")], true)]
#[case::ha_shared("ha", &[("w", "east", "https://w:1", "writer"), ("r", "east", "https://r:1", "replica")], false)]
#[case::duplicate_node("dc", &[("w", "east", "https://w:1", "writer"), ("w", "west", "https://r:1", "replica")], false)]
#[case::duplicate_address("dc", &[("w", "east", "https://same:1", "writer"), ("r", "west", "https://same:1", "replica")], false)]
#[case::no_writer("dc", &[("a", "east", "https://a:1", "replica"), ("b", "west", "https://b:1", "replica")], false)]
#[case::multiple_writers("dc", &[("a", "east", "https://a:1", "writer"), ("b", "west", "https://b:1", "writer")], false)]
#[case::invalid_role("dc", &[("w", "east", "https://w:1", "writer"), ("r", "west", "https://r:1", "observer")], false)]
fn test_config_and_backup_membership_rules_agree(
    #[case] mode: &str,
    #[case] members: &[MembershipMember],
    #[case] expected: bool,
) {
    let configured = config::from_toml("x.toml".into(), &membership_config(mode, members))
        .and_then(|partial| Config::default().apply(partial))
        .is_ok();
    let fixture = valid_backup();
    mutate_manifest(&fixture.backup, |manifest| {
        manifest["availability"]["mode"] = serde_json::json!(mode);
        manifest["availability"]["membership"] = serde_json::json!({
            "group": "g",
            "members": members
                .iter()
                .map(|(node, dc, address, role)| serde_json::json!({
                    "node": node,
                    "dc": dc,
                    "address": address,
                    "role": role,
                }))
                .collect::<Vec<_>>(),
        });
    });

    assert_eq!(
        (configured, backup_verify(&fixture.backup, &mut Vec::new()).is_ok()),
        (expected, expected)
    );
}

#[rstest]
#[case::empty_group(
    serde_json::json!({"group": "", "members": [
        {"node": "a", "dc": "east", "address": "10.0.0.1:1", "role": "writer"}
    ]}),
    "empty group"
)]
#[case::empty_roster(serde_json::json!({"group": "g", "members": []}), "empty roster")]
#[case::duplicate_node(
    serde_json::json!({"group": "g", "members": [
        {"node": "a", "dc": "east", "address": "10.0.0.1:1", "role": "writer"},
        {"node": "a", "dc": "west", "address": "10.0.0.2:1", "role": "replica"}
    ]}),
    "duplicate node a"
)]
#[case::duplicate_dc(
    serde_json::json!({"group": "g", "members": [
        {"node": "a", "dc": "east", "address": "10.0.0.1:1", "role": "writer"},
        {"node": "b", "dc": "east", "address": "10.0.0.2:1", "role": "replica"}
    ]}),
    "duplicate dc east"
)]
#[case::duplicate_address(
    serde_json::json!({"group": "g", "members": [
        {"node": "a", "dc": "east", "address": "10.0.0.1:1", "role": "writer"},
        {"node": "b", "dc": "west", "address": "10.0.0.1:1", "role": "replica"}
    ]}),
    "duplicate address 10.0.0.1:1"
)]
#[case::no_writer(
    serde_json::json!({"group": "g", "members": [
        {"node": "a", "dc": "east", "address": "10.0.0.1:1", "role": "replica"}
    ]}),
    "expected one writer, found 0"
)]
#[case::multiple_writers(
    serde_json::json!({"group": "g", "members": [
        {"node": "a", "dc": "east", "address": "10.0.0.1:1", "role": "writer"},
        {"node": "b", "dc": "west", "address": "10.0.0.2:1", "role": "writer"}
    ]}),
    "expected one writer, found 2"
)]
#[case::invalid_role(
    serde_json::json!({"group": "g", "members": [
        {"node": "a", "dc": "east", "address": "10.0.0.1:1", "role": "writer"},
        {"node": "b", "dc": "west", "address": "10.0.0.2:1", "role": "observer"}
    ]}),
    "invalid role observer"
)]
fn test_verify_rejects_invalid_membership(#[case] membership: serde_json::Value, #[case] expected: &str) {
    let fixture = valid_backup();
    mutate_manifest(&fixture.backup, |manifest| {
        manifest["availability"]["mode"] = serde_json::json!("ha");
        manifest["availability"]["membership"] = membership;
    });
    let mut out = Vec::new();

    backup_verify(&fixture.backup, &mut out).unwrap_err();

    assert!(String::from_utf8(out).unwrap().contains(expected));
}

#[test]
fn test_verify_propagates_report_write_errors() {
    let fixture = valid_backup();
    std::fs::remove_file(fixture.backup.join("config.toml")).unwrap();
    let mut out: &mut [u8] = &mut [];

    let error = backup_verify(&fixture.backup, &mut out).unwrap_err();

    assert_eq!(
        error.downcast_ref::<std::io::Error>().map(std::io::Error::kind),
        Some(std::io::ErrorKind::WriteZero)
    );
}

#[rstest]
#[case::frontier(AvailabilityReport::Frontier)]
#[case::placements(AvailabilityReport::Placements)]
#[case::writer_identity(AvailabilityReport::WriterIdentity)]
#[case::writer_count(AvailabilityReport::WriterCount)]
fn test_verify_propagates_availability_report_write_errors(#[case] report: AvailabilityReport) {
    let fixture = valid_backup();
    mutate_manifest(&fixture.backup, |manifest| match report {
        AvailabilityReport::Frontier => manifest["availability"]["metadata_frontier"] = serde_json::json!(999),
        AvailabilityReport::Placements => manifest["availability"]["placements"] = serde_json::json!(1),
        AvailabilityReport::WriterIdentity => {
            manifest["availability"]["writer_identity"] = serde_json::json!("node-a");
        }
        AvailabilityReport::WriterCount => {
            manifest["availability"]["membership"] = serde_json::json!({
                "group": "g",
                "members": [{
                    "node": "a",
                    "dc": "east",
                    "address": "10.0.0.1:1",
                    "role": "replica"
                }]
            });
        }
    });
    let mut scope_output = [0; b"scope\tecosystems\tcore\n".len()];
    let mut out = scope_output.as_mut_slice();

    let error = backup_verify(&fixture.backup, &mut out).unwrap_err();

    assert_eq!(
        error.downcast_ref::<std::io::Error>().map(std::io::Error::kind),
        Some(std::io::ErrorKind::WriteZero)
    );
}

#[derive(Clone, Copy)]
enum AvailabilityReport {
    Frontier,
    Placements,
    WriterIdentity,
    WriterCount,
}

#[derive(Clone, Copy)]
enum BlobIndexMutation {
    Header,
    Row,
    Digest,
    Size,
    Path,
    Duplicate,
}

fn valid_blob_index(fixture: &BackupFixture) -> String {
    format!(
        "sha256\tsize_bytes\tpath\n{content}\t{content_size}\t{content_path}\n{metadata}\t{metadata_size}\t{metadata_path}\n",
        content = fixture.content_digest.as_str(),
        content_size = b"artifact bytes".len(),
        content_path = blob_relpath(&fixture.content_digest),
        metadata = fixture.metadata_digest.as_str(),
        metadata_size = b"metadata bytes".len(),
        metadata_path = blob_relpath(&fixture.metadata_digest),
    )
}

fn mutated_blob_index(fixture: &BackupFixture, mutation: BlobIndexMutation) -> String {
    let mut index = valid_blob_index(fixture);
    match mutation {
        BlobIndexMutation::Header => index.replace_range(.."sha256\tsize_bytes\tpath".len(), "bad header"),
        BlobIndexMutation::Row => index.push_str("bad-row\n"),
        BlobIndexMutation::Digest => index.push_str("bad\t1\tbad\n"),
        BlobIndexMutation::Size => writeln!(
            index,
            "{}\tbad\t{}",
            fixture.content_digest.as_str(),
            blob_relpath(&fixture.content_digest)
        )
        .unwrap(),
        BlobIndexMutation::Path => index = index.replace(&blob_relpath(&fixture.content_digest), "wrong/path"),
        BlobIndexMutation::Duplicate => writeln!(
            index,
            "{}\t11\t{}",
            fixture.content_digest.as_str(),
            blob_relpath(&fixture.content_digest)
        )
        .unwrap(),
    }
    index
}

fn rewrite_blob_index(fixture: &BackupFixture, index: String) {
    std::fs::write(fixture.backup.join("blobs.tsv"), index).unwrap();
    resign_file(&fixture.backup, "blob_index", "blobs.tsv");
}
