use std::path::Path;

use peryx_ecosystem_registry::pypi::store::PypiStore as _;
use peryx_storage::blob::Digest;
use peryx_storage::meta::MetaStore;
use rstest::rstest;

use crate::operator;

use super::{FailOnLine, blob_relpath, identified_backup, valid_backup};

#[test]
fn test_backup_verify_reports_ok_for_valid_backup() {
    let (_source, _root, _config, backup, _content_digest, _metadata_digest) = valid_backup();
    let metadata = backup.join("metadata/peryx.redb");
    let before = std::fs::read(&metadata).unwrap();

    let mut out = Vec::new();
    operator::backup_verify(&backup, &mut out).unwrap();
    operator::backup_verify(&backup, &mut out).unwrap();

    assert_eq!(out, b"ok\nok\n");
    assert_eq!(std::fs::read(metadata).unwrap(), before);
}

#[test]
fn test_backup_verify_reports_missing_blob() {
    let (_source, _root, _config, backup, content_digest, _metadata_digest) = valid_backup();
    std::fs::remove_file(backup.join(blob_relpath(&content_digest))).unwrap();

    let mut out = Vec::new();
    let err = operator::backup_verify(&backup, &mut out).unwrap_err();

    assert!(err.to_string().contains("backup verification failed"));
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains(&format!("problem\tblob\t{}\tmissing", content_digest.as_str()))
    );
}

#[test]
fn test_backup_verify_reports_mismatched_blob() {
    let (_source, _root, _config, backup, content_digest, _metadata_digest) = valid_backup();
    std::fs::write(backup.join(blob_relpath(&content_digest)), b"tampered").unwrap();

    let mut out = Vec::new();
    let err = operator::backup_verify(&backup, &mut out).unwrap_err();

    assert!(err.to_string().contains("backup verification failed"));
    assert!(String::from_utf8(out).unwrap().contains("sha256 expected"));
}

#[test]
fn test_backup_verify_rejects_unsupported_manifest_format() {
    let (_source, _root, _config, backup, _content_digest, _metadata_digest) = valid_backup();
    mutate_manifest(&backup, |manifest| manifest["format"] = serde_json::json!(3));

    let err = operator::backup_verify(&backup, &mut Vec::new()).unwrap_err();

    assert!(err.to_string().contains("unsupported backup format 3"));
}

#[test]
fn test_backup_verify_reports_missing_metadata_store() {
    let (_source, _root, _config, backup, _content_digest, _metadata_digest) = valid_backup();
    std::fs::remove_file(backup.join("metadata/peryx.redb")).unwrap();

    let mut out = Vec::new();
    let err = operator::backup_verify(&backup, &mut out).unwrap_err();

    assert!(err.to_string().contains("backup verification failed"));
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("problem\tmetadata\tmetadata/peryx.redb\tmissing")
    );
}

#[test]
fn test_backup_verify_reports_missing_manifest_files() {
    let (_source, _root, _config, backup, _content_digest, _metadata_digest) = valid_backup();
    std::fs::remove_file(backup.join("config.toml")).unwrap();
    std::fs::remove_file(backup.join("blobs.tsv")).unwrap();

    let mut out = Vec::new();
    let err = operator::backup_verify(&backup, &mut out).unwrap_err();

    let text = String::from_utf8(out).unwrap();
    assert!(err.to_string().contains("backup verification failed"));
    assert!(text.contains("problem\tconfig\tconfig.toml\tmissing"));
    assert!(text.contains("problem\tblob-index\tblobs.tsv\tmissing"));
}

#[test]
fn test_backup_verify_reports_corrupt_metadata_store() {
    let (_source, _root, _config, backup, _content_digest, _metadata_digest) = valid_backup();
    std::fs::write(backup.join("metadata/peryx.redb"), b"not a redb database").unwrap();
    resign_metadata(&backup);

    let mut out = Vec::new();
    let err = operator::backup_verify(&backup, &mut out).unwrap_err();

    assert!(err.to_string().contains("backup verification failed"));
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("problem\tmetadata\tmetadata/peryx.redb")
    );
}

#[cfg(unix)]
#[test]
fn test_backup_verify_reports_unreadable_metadata_store() {
    use std::os::unix::fs::PermissionsExt as _;

    let (_source, _root, _config, backup, _content_digest, _metadata_digest) = valid_backup();
    let path = backup.join("metadata/peryx.redb");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();

    let mut out = Vec::new();
    let result = operator::backup_verify(&backup, &mut out);
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let err = result.unwrap_err();
    let text = String::from_utf8(out).unwrap();

    assert!(err.to_string().contains("backup verification failed"));
    assert!(text.starts_with("problem\tmetadata\tmetadata/peryx.redb\tI/O error: Permission denied"));
    assert!(text.ends_with("\nproblems\t1\n"));
    assert_eq!(text.lines().count(), 2);
}

#[cfg(unix)]
#[rstest]
#[case::config("config.toml")]
#[case::blob_index("blobs.tsv")]
fn test_backup_verify_propagates_unreadable_manifest_file(#[case] relative: &str) {
    use std::os::unix::fs::PermissionsExt as _;

    let (_source, _root, _config, backup, _content_digest, _metadata_digest) = valid_backup();
    let path = backup.join(relative);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();

    let mut out = Vec::new();
    let result = operator::backup_verify(&backup, &mut out);
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let err = result.unwrap_err();

    assert_eq!(
        err.downcast_ref::<std::io::Error>().map(std::io::Error::kind),
        Some(std::io::ErrorKind::PermissionDenied)
    );
    assert!(out.is_empty());
}

#[test]
fn test_backup_verify_reports_non_regular_metadata_store() {
    let (_source, _root, _config, backup, _content_digest, _metadata_digest) = valid_backup();
    let path = backup.join("metadata/peryx.redb");
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();

    let mut out = Vec::new();
    let err = operator::backup_verify(&backup, &mut out).unwrap_err();

    assert!(err.to_string().contains("backup verification failed"));
    assert_eq!(out, b"problem\tmetadata\tmetadata/peryx.redb\tmissing\nproblems\t1\n");
}

#[rstest]
#[case::valid_but_modified(modify_metadata_store, "sha256 expected")]
#[case::size_mismatch(modify_metadata_size, "size expected 0")]
fn test_backup_verify_reports_metadata_mismatch(#[case] modify: fn(&Path), #[case] expected: &str) {
    let (_source, _root, _config, backup, _content_digest, _metadata_digest) = valid_backup();
    modify(&backup);

    let mut out = Vec::new();
    let err = operator::backup_verify(&backup, &mut out).unwrap_err();

    assert!(err.to_string().contains("backup verification failed"));
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains(&format!("problem\tmetadata\tmetadata/peryx.redb\t{expected}"))
    );
}

#[rstest]
#[case::missing_config(remove_config)]
#[case::missing_blob_index(remove_blob_index)]
#[case::missing_metadata(remove_metadata)]
#[case::sha_mismatch(modify_metadata_store)]
#[case::size_mismatch(modify_metadata_size)]
fn test_backup_verify_propagates_report_error(#[case] modify: fn(&Path)) {
    let (_source, _root, _config, backup, _content_digest, _metadata_digest) = valid_backup();
    modify(&backup);

    let mut out: &mut [u8] = &mut [];
    let err = operator::backup_verify(&backup, &mut out).unwrap_err();

    assert_eq!(
        err.downcast_ref::<std::io::Error>().map(std::io::Error::kind),
        Some(std::io::ErrorKind::WriteZero)
    );
}

#[cfg(unix)]
#[test]
fn test_backup_verify_propagates_unreadable_metadata_report_error() {
    use std::os::unix::fs::PermissionsExt as _;

    let (_source, _root, _config, backup, _content_digest, _metadata_digest) = valid_backup();
    let path = backup.join("metadata/peryx.redb");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();

    let mut out: &mut [u8] = &mut [];
    let result = operator::backup_verify(&backup, &mut out);
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let err = result.unwrap_err();

    assert_eq!(
        err.downcast_ref::<std::io::Error>().map(std::io::Error::kind),
        Some(std::io::ErrorKind::WriteZero)
    );
}

#[test]
fn test_backup_verify_reports_manifest_file_mismatch() {
    let (_source, _root, _config, backup, _content_digest, _metadata_digest) = valid_backup();
    std::fs::write(backup.join("config.toml"), b"tampered config").unwrap();

    let mut out = Vec::new();
    let err = operator::backup_verify(&backup, &mut out).unwrap_err();

    let text = String::from_utf8(out).unwrap();
    assert!(err.to_string().contains("backup verification failed"));
    assert!(text.contains("problem\tconfig\tconfig.toml\tsha256 expected"));
    assert!(text.contains("problem\tconfig\tconfig.toml\tsize expected"));
}

#[test]
fn test_backup_verify_reports_blob_index_problems() {
    let (_source, _root, _config, backup, content_digest, _metadata_digest) = valid_backup();
    std::fs::write(
        backup.join("blobs.tsv"),
        format!(
            "bad header\n\nbad-row\nbad\t1\tbad\n{digest}\tbad\t{path}\n{digest}\t11\twrong/path\n{digest}\t11\t{path}\n{digest}\t11\t{path}\n",
            digest = content_digest.as_str(),
            path = blob_relpath(&content_digest),
        ),
    )
    .unwrap();

    let mut out = Vec::new();
    let err = operator::backup_verify(&backup, &mut out).unwrap_err();

    let text = String::from_utf8(out).unwrap();
    assert!(err.to_string().contains("backup verification failed"));
    assert!(text.contains("problem\tblob-index\theader\tinvalid header"));
    assert!(text.contains("problem\tblob-index\tline 3\tinvalid row"));
    assert!(text.contains("problem\tblob-index\tline 4\tinvalid digest"));
    assert!(text.contains(&format!(
        "problem\tblob-index\t{}\tinvalid size",
        content_digest.as_str()
    )));
    assert!(text.contains("invalid size"));
    assert!(text.contains("invalid path"));
    assert!(text.contains("duplicate digest"));
    assert!(text.contains("missing referenced digest"));
    assert!(text.contains("problem\tblob-index\tcount"));
    assert!(text.contains("problem\tblob-index\tbytes"));
}

#[test]
fn test_backup_verify_reports_blob_size_mismatch() {
    let (_source, _root, _config, backup, content_digest, metadata_digest) = valid_backup();
    std::fs::write(
        backup.join("blobs.tsv"),
        format!(
            "sha256\tsize_bytes\tpath\n{content}\t999\t{content_path}\n{metadata}\t14\t{metadata_path}\n",
            content = content_digest.as_str(),
            content_path = blob_relpath(&content_digest),
            metadata = metadata_digest.as_str(),
            metadata_path = blob_relpath(&metadata_digest),
        ),
    )
    .unwrap();

    let mut out = Vec::new();
    let err = operator::backup_verify(&backup, &mut out).unwrap_err();

    assert!(err.to_string().contains("backup verification failed"));
    assert!(String::from_utf8(out).unwrap().contains("size expected 999"));
}

fn modify_metadata_store(backup: &Path) {
    MetaStore::open_existing(backup.join("metadata/peryx.redb"))
        .unwrap()
        .put_upload("hosted", "broken", "broken.whl", b"invalid")
        .unwrap();
}

fn remove_config(backup: &Path) {
    std::fs::remove_file(backup.join("config.toml")).unwrap();
}

fn remove_blob_index(backup: &Path) {
    std::fs::remove_file(backup.join("blobs.tsv")).unwrap();
}

fn remove_metadata(backup: &Path) {
    std::fs::remove_file(backup.join("metadata/peryx.redb")).unwrap();
}

fn modify_metadata_size(backup: &Path) {
    mutate_manifest(backup, |manifest| {
        manifest["metadata"]["size_bytes"] = serde_json::json!(0);
    });
}

fn resign_metadata(backup: &Path) {
    let bytes = std::fs::read(backup.join("metadata/peryx.redb")).unwrap();
    mutate_manifest(backup, |manifest| {
        manifest["metadata"]["sha256"] = serde_json::json!(Digest::of(&bytes).as_str());
        manifest["metadata"]["size_bytes"] = serde_json::json!(bytes.len());
    });
}

fn mutate_manifest(backup: &Path, mutate: impl FnOnce(&mut serde_json::Value)) {
    let path = backup.join("manifest.json");
    let mut manifest = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    mutate(&mut manifest);
    std::fs::write(&path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
}

#[test]
fn test_backup_verify_rejects_stale_metadata_frontier() {
    let (_source, _root, _config, backup, _content_digest, _metadata_digest) = valid_backup();
    mutate_manifest(&backup, |manifest| {
        manifest["availability"]["metadata_frontier"] = serde_json::json!(999);
    });

    let mut out = Vec::new();
    let err = operator::backup_verify(&backup, &mut out).unwrap_err();

    assert!(err.to_string().contains("backup verification failed"));
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("problem\tavailability\tfrontier\texpected 999, found ")
    );
}

#[test]
fn test_backup_verify_rejects_a_tampered_writer_identity() {
    let (_holder, backup) = identified_backup("node-a", 1);
    mutate_manifest(&backup, |manifest| {
        manifest["availability"]["writer_identity"] = serde_json::json!("node-x");
    });

    let mut out = Vec::new();
    let err = operator::backup_verify(&backup, &mut out).unwrap_err();

    assert!(err.to_string().contains("backup verification failed"));
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("problem\tavailability\twriter-identity\texpected node-x, found node-a")
    );
}

#[test]
fn test_backup_verify_propagates_writer_identity_report_error() {
    let (_holder, backup) = identified_backup("node-a", 1);
    mutate_manifest(&backup, |manifest| {
        manifest["availability"]["writer_identity"] = serde_json::json!("node-x");
    });

    let mut out = FailOnLine {
        needle: "\n",
        ..FailOnLine::default()
    };
    operator::backup_verify(&backup, &mut out).unwrap_err();

    assert!(
        out.seen
            .contains("problem\tavailability\twriter-identity\texpected node-x, found node-a"),
        "{}",
        out.seen
    );
}

#[test]
fn test_backup_verify_rejects_mismatched_placement_count() {
    let (_source, _root, _config, backup, _content_digest, _metadata_digest) = valid_backup();
    mutate_manifest(&backup, |manifest| {
        manifest["availability"]["placements"] = serde_json::json!(5);
    });

    let mut out = Vec::new();
    let err = operator::backup_verify(&backup, &mut out).unwrap_err();

    assert!(err.to_string().contains("backup verification failed"));
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("problem\tavailability\tplacements\texpected 5, found 0")
    );
}

#[rstest]
#[case::empty_roster(serde_json::json!({"group": "g", "members": []}), "empty roster")]
#[case::two_writers(
    serde_json::json!({"group": "g", "members": [
        {"node": "a", "dc": "east", "address": "10.0.0.1:1", "role": "writer"},
        {"node": "b", "dc": "west", "address": "10.0.0.2:1", "role": "writer"},
    ]}),
    "expected one writer, found 2"
)]
#[case::duplicate_identity(
    serde_json::json!({"group": "", "members": [
        {"node": "a", "dc": "east", "address": "10.0.0.1:1", "role": "replica"},
        {"node": "a", "dc": "east", "address": "10.0.0.1:1", "role": "replica"},
    ]}),
    "duplicate node a"
)]
fn test_backup_verify_rejects_malformed_membership(#[case] membership: serde_json::Value, #[case] expected: &str) {
    let (_source, _root, _config, backup, _content_digest, _metadata_digest) = valid_backup();
    mutate_manifest(&backup, |manifest| manifest["availability"]["membership"] = membership);

    let mut out = Vec::new();
    let err = operator::backup_verify(&backup, &mut out).unwrap_err();

    assert!(err.to_string().contains("backup verification failed"));
    assert!(String::from_utf8(out).unwrap().contains(expected), "{expected}");
}

#[test]
fn test_backup_verify_propagates_frontier_report_error() {
    let (_source, _root, _config, backup, _content_digest, _metadata_digest) = valid_backup();
    mutate_manifest(&backup, |manifest| {
        manifest["availability"]["metadata_frontier"] = serde_json::json!(999);
    });

    let mut out = FailOnLine {
        needle: "\n",
        ..FailOnLine::default()
    };
    operator::backup_verify(&backup, &mut out).unwrap_err();

    assert!(
        out.seen
            .contains("problem\tavailability\tfrontier\texpected 999, found "),
        "{}",
        out.seen
    );
}

#[test]
fn test_backup_verify_propagates_placements_report_error() {
    let (_source, _root, _config, backup, _content_digest, _metadata_digest) = valid_backup();
    mutate_manifest(&backup, |manifest| {
        manifest["availability"]["placements"] = serde_json::json!(5);
    });

    let mut out = FailOnLine {
        needle: "\n",
        ..FailOnLine::default()
    };
    operator::backup_verify(&backup, &mut out).unwrap_err();

    assert!(
        out.seen
            .contains("problem\tavailability\tplacements\texpected 5, found 0"),
        "{}",
        out.seen
    );
}

#[test]
fn test_backup_verify_propagates_membership_report_error() {
    let (_source, _root, _config, backup, _content_digest, _metadata_digest) = valid_backup();
    mutate_manifest(&backup, |manifest| {
        manifest["availability"]["membership"] = serde_json::json!({"group": "g", "members": [
            {"node": "a", "dc": "east", "address": "10.0.0.1:1", "role": "writer"},
            {"node": "b", "dc": "west", "address": "10.0.0.2:1", "role": "writer"},
        ]});
    });

    let mut out = FailOnLine {
        needle: "\n",
        ..FailOnLine::default()
    };
    operator::backup_verify(&backup, &mut out).unwrap_err();

    assert!(
        out.seen
            .contains("problem\tavailability\tmembership\texpected one writer, found 2"),
        "{}",
        out.seen
    );
}
