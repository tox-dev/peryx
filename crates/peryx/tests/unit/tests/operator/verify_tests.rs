use std::fmt::Write as _;

use rstest::rstest;

use crate::config::{self, Config};
use crate::operator;

use super::support::{
    BackupFixture, backup_verify, blob_relpath, identified_backup, mutate_manifest, resign_file, valid_backup,
};

#[test]
fn test_verify_accepts_a_complete_backup_without_mutation() {
    let fixture = valid_backup();
    let metadata = fixture.backup.join("metadata/peryx.redb");
    let before = std::fs::read(&metadata).unwrap();
    let mut out = Vec::new();

    backup_verify(&fixture.backup, &mut out).unwrap();

    assert_eq!((out, std::fs::read(metadata).unwrap()), (b"ok\n".to_vec(), before));
}

#[rstest]
#[case::missing(false, "missing")]
#[case::mismatched(true, "sha256 expected")]
fn test_verify_reports_blob_failures(#[case] tamper: bool, #[case] expected: &str) {
    let fixture = valid_backup();
    let blob = fixture.backup.join(blob_relpath(&fixture.content_digest));
    if tamper {
        std::fs::write(blob, b"tampered").unwrap();
    } else {
        std::fs::remove_file(blob).unwrap();
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
#[case::config("config", "config.toml")]
#[case::metadata("metadata", "metadata/peryx.redb")]
#[case::blob_index("blob_index", "blobs.tsv")]
fn test_verify_rejects_invalid_manifest_paths(#[case] field: &str, #[case] expected: &str) {
    let fixture = valid_backup();
    mutate_manifest(&fixture.backup, |manifest| {
        manifest[field]["path"] = serde_json::json!("../outside");
    });

    let error = backup_verify(&fixture.backup, &mut Vec::new()).unwrap_err();

    assert_eq!(
        error.to_string(),
        format!(
            "invalid {} path \"../outside\"; expected {expected:?}",
            field.replace('_', " ")
        )
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

    assert_eq!(out, b"ok\n");
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
    let mut out: &mut [u8] = &mut [];

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
