use super::*;

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, meta)
}

#[test]
fn test_tag_page_round_trips_with_and_without_a_link() {
    let (_dir, meta) = store();
    set_tag_page(&meta, "hub", "library/nginx", "", 42, Some("</v2/x?n=1>"), b"{}").unwrap();
    assert_eq!(
        tag_page(&meta, "hub", "library/nginx", "").unwrap(),
        Some((42, Some("</v2/x?n=1>".to_owned()), b"{}".to_vec()))
    );

    set_tag_page(&meta, "hub", "library/nginx", "n=1", 7, None, b"[]").unwrap();
    assert_eq!(
        tag_page(&meta, "hub", "library/nginx", "n=1").unwrap(),
        Some((7, None, b"[]".to_vec()))
    );
}

#[test]
fn test_a_truncated_tag_page_record_reads_as_absent() {
    let (_dir, meta) = store();
    // Corrupt pages read as absent to force a clean refetch.
    for raw in [
        vec![0u8; 4],
        vec![0u8; 10],
        [&0i64.to_be_bytes()[..], &99u32.to_be_bytes()[..], b"x"].concat(),
    ] {
        meta.put_driver_value(&tag_page_key("hub", "repo", ""), &raw).unwrap();
        assert_eq!(tag_page(&meta, "hub", "repo", "").unwrap(), None, "{raw:?}");
    }
}

#[test]
fn test_manifest_round_trips_through_the_store() {
    let (_dir, meta) = store();
    let manifest = Manifest {
        media_type: "application/vnd.oci.image.manifest.v1+json".to_owned(),
        bytes: b"{\"schemaVersion\":2}".to_vec(),
    };
    record_manifest(&meta, "hub", "app", "sha256:abc", &manifest).unwrap();
    assert_eq!(get_manifest(&meta, "sha256:abc").unwrap(), Some(manifest));
    assert_eq!(get_manifest(&meta, "sha256:missing").unwrap(), None);
}

#[test]
fn test_origin_maps_to_the_neutral_source() {
    assert_eq!(OciArtifactOrigin::Pushed.artifact_source(), ArtifactSource::Hosted);
    assert_eq!(OciArtifactOrigin::Mirrored.artifact_source(), ArtifactSource::Proxy);
}

#[test]
fn test_content_placement_records_local_availability() {
    use peryx_ha::ByteAvailability;

    let (_dir, meta) = store();
    assert!(!content_available_locally(&meta, "sha256:missing").unwrap());
    record_content_placement(&meta, "sha256:abc", OciArtifactOrigin::Pushed, true).unwrap();
    assert!(content_available_locally(&meta, "sha256:abc").unwrap());
    assert_eq!(
        meta.get_artifact_placement("sha256:abc").unwrap().unwrap().availability,
        ByteAvailability::Local
    );
}

#[test]
fn test_decode_rejects_truncated_manifest() {
    assert_eq!(Manifest::decode(&[0x00]), None);
    assert_eq!(Manifest::decode(&[0x00, 0x05, b'a']), None);
}

#[test]
fn test_tag_freshness_round_trips_and_rejects_corrupt_records() {
    let (_dir, meta) = store();
    assert_eq!(tag_freshness(&meta, "hub", "repo", "latest").unwrap(), None);
    set_tag_freshness(&meta, "hub", "repo", "latest", "sha256:abc", 1234).unwrap();
    assert_eq!(
        tag_freshness(&meta, "hub", "repo", "latest").unwrap(),
        Some((1234, "sha256:abc".to_owned()))
    );
    meta.put_driver_value(&tag_freshness_key("hub", "repo", "short"), &[0x00])
        .unwrap();
    assert_eq!(tag_freshness(&meta, "hub", "repo", "short").unwrap(), None);
    let mut corrupt = 5i64.to_be_bytes().to_vec();
    corrupt.push(0xff);
    meta.put_driver_value(&tag_freshness_key("hub", "repo", "badutf"), &corrupt)
        .unwrap();
    assert_eq!(tag_freshness(&meta, "hub", "repo", "badutf").unwrap(), None);
    put_tag(&meta, "hub", "repo", "latest", "sha256:abc").unwrap();
    delete_tag(&meta, "hub", "repo", "latest").unwrap();
    assert_eq!(tag_freshness(&meta, "hub", "repo", "latest").unwrap(), None);
}

#[test]
fn test_tags_scope_to_index_and_repo_and_sort() {
    let (_dir, meta) = store();
    put_tag(&meta, "hub", "library/nginx", "latest", "sha256:1").unwrap();
    put_tag(&meta, "hub", "library/nginx", "1.25", "sha256:2").unwrap();
    put_tag(&meta, "hub", "library/other", "latest", "sha256:3").unwrap();
    put_tag(&meta, "gitlab", "library/nginx", "edge", "sha256:9").unwrap();
    assert_eq!(
        get_tag(&meta, "hub", "library/nginx", "latest").unwrap(),
        Some("sha256:1".to_owned())
    );
    assert_eq!(get_tag(&meta, "hub", "library/nginx", "absent").unwrap(), None);
    assert_eq!(
        list_tags(&meta, "hub", "library/nginx").unwrap(),
        vec!["1.25", "latest"]
    );
    assert_eq!(
        list_tag_targets(&meta, "hub", "library/nginx").unwrap(),
        vec![
            ("1.25".to_owned(), "sha256:2".to_owned()),
            ("latest".to_owned(), "sha256:1".to_owned()),
        ]
    );
}

#[test]
fn test_put_tag_reports_insert_and_repoints() {
    let (_dir, meta) = store();

    assert_eq!(
        (
            put_tag(&meta, "hub", "library/nginx", "latest", "sha256:1").unwrap(),
            put_tag(&meta, "hub", "library/nginx", "latest", "sha256:2").unwrap(),
            put_tag(&meta, "hub", "library/nginx", "latest", "sha256:2").unwrap(),
            get_tag(&meta, "hub", "library/nginx", "latest").unwrap()
        ),
        (true, true, false, Some("sha256:2".to_owned()))
    );
}

#[test]
fn test_referrers_scope_to_index_repo_and_subject() {
    let (_dir, meta) = store();
    put_referrer(
        &meta,
        "store",
        "app",
        "sha256:subj",
        "sha256:ref1",
        b"{\"digest\":\"sha256:ref1\"}",
    )
    .unwrap();
    put_referrer(
        &meta,
        "store",
        "app",
        "sha256:subj",
        "sha256:ref2",
        b"{\"digest\":\"sha256:ref2\"}",
    )
    .unwrap();
    put_referrer(
        &meta,
        "store",
        "other",
        "sha256:subj",
        "sha256:ref3",
        b"{\"digest\":\"sha256:ref3\"}",
    )
    .unwrap();
    put_referrer(&meta, "store", "app", "sha256:elsewhere", "sha256:ref4", b"{}").unwrap();

    let referrers = list_referrers(&meta, "store", "app", "sha256:subj").unwrap();
    assert_eq!(referrers.len(), 2);
    assert!(referrers.iter().any(|value| value == b"{\"digest\":\"sha256:ref1\"}"));
    assert!(referrers.iter().any(|value| value == b"{\"digest\":\"sha256:ref2\"}"));
    assert!(list_referrers(&meta, "store", "app", "sha256:none").unwrap().is_empty());
}

fn index_of(child: &str) -> Manifest {
    Manifest {
        media_type: "application/vnd.oci.image.index.v1+json".to_owned(),
        bytes: format!(r#"{{"manifests":[{{"digest":"{child}"}}]}}"#).into_bytes(),
    }
}

#[test]
fn test_record_manifest_marks_the_manifest_and_its_index_children() {
    let (_dir, meta) = store();
    let child = format!("sha256:{}", "c".repeat(64));
    record_manifest(&meta, "store", "app", "sha256:idx", &index_of(&child)).unwrap();
    assert!(manifest_is_member(&meta, "store", "app", "sha256:idx").unwrap());
    assert!(manifest_is_member(&meta, "store", "app", &child).unwrap());
    assert!(!manifest_is_member(&meta, "store", "other", "sha256:idx").unwrap());
    assert!(!manifest_is_member(&meta, "store", "app", "sha256:absent").unwrap());
}

#[test]
fn test_blob_membership_is_repository_scoped() {
    let (_dir, meta, digest) = blob_member();

    assert_eq!(
        (
            blob_is_member(&meta, "store", "app", &digest).unwrap(),
            blob_is_member(&meta, "store", "other", &digest).unwrap(),
        ),
        (true, false)
    );
}

#[test]
fn test_record_manifest_marks_its_blob_descriptors() {
    let (_dir, meta) = store();
    let config = format!("sha256:{}", "a".repeat(64));
    let layer = format!("sha256:{}", "b".repeat(64));
    let manifest = Manifest {
        media_type: "application/vnd.oci.image.manifest.v1+json".to_owned(),
        bytes: format!(r#"{{"config":{{"digest":"{config}"}},"layers":[{{"digest":"{layer}"}}]}}"#).into_bytes(),
    };

    record_manifest(&meta, "store", "app", "sha256:manifest", &manifest).unwrap();

    assert_eq!(
        (
            blob_is_member(&meta, "store", "app", &config).unwrap(),
            blob_is_member(&meta, "store", "app", &layer).unwrap(),
            blob_is_member(&meta, "store", "other", &config).unwrap(),
        ),
        (true, true, false)
    );
}

fn image(bytes: &str) -> Manifest {
    Manifest {
        media_type: "application/vnd.oci.image.manifest.v1+json".to_owned(),
        bytes: bytes.as_bytes().to_vec(),
    }
}

fn info() -> TrashInfo {
    TrashInfo {
        deleted_at_unix: 100,
        actor: Some("alice".to_owned()),
        reason: Some("bad build".to_owned()),
    }
}

#[test]
fn test_trash_records_lists_a_trashed_tag_with_provenance_and_retention() {
    let (_dir, meta) = store();
    record_manifest(&meta, "hub", "library/nginx", "sha256:a", &image("{}")).unwrap();
    put_tag(&meta, "hub", "library/nginx", "latest", "sha256:a").unwrap();
    trash_tag(&meta, "hub", "library/nginx", "latest", &info(), false).unwrap();

    let records = trash_records(&meta, "hub").unwrap();

    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.ecosystem, crate::ECOSYSTEM);
    assert_eq!(record.repository.as_str(), "hub");
    assert_eq!(record.resource.as_str(), "library/nginx");
    assert_eq!(
        record.artifact.as_ref().map(peryx_core::ArtifactKey::as_str),
        Some("latest")
    );
    assert_eq!(record.digest.as_deref(), Some("sha256:a"));
    assert_eq!(record.reason.as_deref(), Some("bad build"));
    assert_eq!(record.actor.as_deref(), Some("alice"));
    assert_eq!(record.deleted_at_unix, 100);
    assert!(record.retained, "the manifest content is still stored");
}

#[test]
fn test_trash_records_reports_an_untagged_digest_deletion_once() {
    let (_dir, meta) = store();
    record_manifest(&meta, "hub", "library/nginx", "sha256:a", &image("{}")).unwrap();
    trash_manifest(&meta, "hub", "library/nginx", "sha256:a", &info(), false).unwrap();

    let records = trash_records(&meta, "hub").unwrap();

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].artifact, None, "an untagged digest carries no tag");
    assert_eq!(records[0].digest.as_deref(), Some("sha256:a"));
}

#[test]
fn test_trash_records_does_not_double_count_a_tagged_manifest_deletion() {
    let (_dir, meta) = store();
    record_manifest(&meta, "hub", "app", "sha256:a", &image("{}")).unwrap();
    put_tag(&meta, "hub", "app", "1.0", "sha256:a").unwrap();
    put_tag(&meta, "hub", "app", "latest", "sha256:a").unwrap();
    trash_manifest(&meta, "hub", "app", "sha256:a", &info(), false).unwrap();

    let mut artifacts: Vec<Option<String>> = trash_records(&meta, "hub")
        .unwrap()
        .into_iter()
        .map(|record| record.artifact.map(|artifact| artifact.to_string()))
        .collect();
    artifacts.sort();

    assert_eq!(
        artifacts,
        vec![Some("1.0".to_owned()), Some("latest".to_owned())],
        "the two captured tags are the only records, not a third digest row"
    );
}

#[test]
fn test_trash_records_marks_purged_content_as_not_retained() {
    let (_dir, meta) = store();
    put_tag(&meta, "hub", "app", "latest", "sha256:a").unwrap();
    trash_tag(&meta, "hub", "app", "latest", &info(), false).unwrap();

    let records = trash_records(&meta, "hub").unwrap();

    assert_eq!(records.len(), 1);
    assert!(!records[0].retained, "purged content is not restorable");
}

#[test]
fn test_trash_records_scope_to_one_index_and_skip_corrupt_rows() {
    let (_dir, meta) = store();
    record_manifest(&meta, "hub", "app", "sha256:a", &image("{}")).unwrap();
    put_tag(&meta, "hub", "app", "latest", "sha256:a").unwrap();
    trash_tag(&meta, "hub", "app", "latest", &info(), false).unwrap();
    meta.put_driver_value(&tag_trash_key("hub", "app", "corrupt"), b"not json")
        .unwrap();

    assert_eq!(
        trash_records(&meta, "hub").unwrap().len(),
        1,
        "the corrupt row is skipped"
    );
    assert!(
        trash_records(&meta, "other").unwrap().is_empty(),
        "records scope to the index"
    );
}

#[test]
fn test_restore_tag_reports_a_missing_tag() {
    let (_dir, meta) = store();
    assert_eq!(
        restore_tag(&meta, "hub", "app", "absent", false).unwrap(),
        RestoreTagOutcome::Missing
    );
}

#[test]
fn test_restore_tag_without_a_manifest_record_restores_the_tag() {
    let (_dir, meta) = store();
    record_manifest(&meta, "hub", "app", "sha256:a", &image("{}")).unwrap();
    put_tag(&meta, "hub", "app", "v1", "sha256:a").unwrap();
    trash_tag(&meta, "hub", "app", "v1", &info(), false).unwrap();

    assert_eq!(
        restore_tag(&meta, "hub", "app", "v1", false).unwrap(),
        RestoreTagOutcome::Restored {
            digest: "sha256:a".to_owned()
        }
    );
    assert_eq!(get_tag(&meta, "hub", "app", "v1").unwrap(), Some("sha256:a".to_owned()));
}

#[test]
fn test_restore_tag_keeps_shared_manifest_trash_until_the_last_tag() {
    let (_dir, meta) = store();
    record_manifest(&meta, "hub", "app", "sha256:a", &image("{}")).unwrap();
    put_tag(&meta, "hub", "app", "v1", "sha256:a").unwrap();
    put_tag(&meta, "hub", "app", "v2", "sha256:a").unwrap();
    trash_manifest(&meta, "hub", "app", "sha256:a", &info(), false).unwrap();

    assert_eq!(
        restore_tag(&meta, "hub", "app", "v1", false).unwrap(),
        RestoreTagOutcome::Restored {
            digest: "sha256:a".to_owned()
        }
    );
    assert_eq!(get_tag(&meta, "hub", "app", "v1").unwrap(), Some("sha256:a".to_owned()));
    assert!(manifest_is_trashed(&meta, "hub", "app", "sha256:a").unwrap());
    assert_eq!(list_trashed_tags(&meta, "hub", "app").unwrap(), vec!["v2"]);

    assert_eq!(
        restore_tag(&meta, "hub", "app", "v2", false).unwrap(),
        RestoreTagOutcome::Restored {
            digest: "sha256:a".to_owned()
        }
    );
    assert!(!manifest_is_trashed(&meta, "hub", "app", "sha256:a").unwrap());
    assert!(list_trashed_tags(&meta, "hub", "app").unwrap().is_empty());
}

#[test]
fn test_restore_tag_leaves_an_independent_untagged_deletion() {
    let (_dir, meta) = store();
    record_manifest(&meta, "hub", "app", "sha256:a", &image("{}")).unwrap();
    put_tag(&meta, "hub", "app", "v1", "sha256:a").unwrap();
    trash_tag(&meta, "hub", "app", "v1", &info(), false).unwrap();
    trash_manifest(&meta, "hub", "app", "sha256:a", &info(), false).unwrap();

    restore_tag(&meta, "hub", "app", "v1", false).unwrap();

    assert_eq!(get_tag(&meta, "hub", "app", "v1").unwrap(), Some("sha256:a".to_owned()));
    assert!(
        manifest_is_trashed(&meta, "hub", "app", "sha256:a").unwrap(),
        "the untagged digest deletion is not this tag's to undo"
    );
}

fn blob_member() -> (tempfile::TempDir, MetaStore, String) {
    let (dir, meta) = store();
    let digest = format!("sha256:{}", "a".repeat(64));
    record_blob_membership(&meta, "store", "app", &digest).unwrap();
    (dir, meta, digest)
}
