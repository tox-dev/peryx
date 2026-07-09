use crate::operator;

use super::{blob_relpath, valid_backup};

#[test]
fn test_backup_verify_reports_ok_for_valid_backup() {
    let (_source, _root, _config, backup, _content_digest, _metadata_digest) = valid_backup();

    let mut out = Vec::new();
    operator::backup_verify(&backup, &mut out).unwrap();

    assert!(String::from_utf8(out).unwrap().contains("ok\n"));
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
    mutate_manifest(&backup, |manifest| manifest["format"] = serde_json::json!(2));

    let err = operator::backup_verify(&backup, &mut Vec::new()).unwrap_err();

    assert!(err.to_string().contains("unsupported backup format 2"));
}

#[test]
fn test_backup_verify_reports_missing_metadata_store() {
    let (_source, _root, _config, backup, _content_digest, _metadata_digest) = valid_backup();
    std::fs::remove_file(backup.join("metadata/velodex.redb")).unwrap();

    let mut out = Vec::new();
    let err = operator::backup_verify(&backup, &mut out).unwrap_err();

    assert!(err.to_string().contains("backup verification failed"));
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("problem\tmetadata\tmetadata/velodex.redb\tmissing")
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
    std::fs::write(backup.join("metadata/velodex.redb"), b"not a redb database").unwrap();

    let mut out = Vec::new();
    let err = operator::backup_verify(&backup, &mut out).unwrap_err();

    assert!(err.to_string().contains("backup verification failed"));
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("problem\tmetadata\tmetadata/velodex.redb")
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

fn mutate_manifest(backup: &std::path::Path, mutate: impl FnOnce(&mut serde_json::Value)) {
    let path = backup.join("manifest.json");
    let mut manifest = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    mutate(&mut manifest);
    std::fs::write(&path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
}
