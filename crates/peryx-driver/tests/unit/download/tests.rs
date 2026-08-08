use super::{BlobTail, DownloadRegistry};

#[test]
fn test_distinct_digests_keep_independent_producers() {
    let registry = DownloadRegistry::default();
    let (_, first) = registry.register("first", Option::<BlobTail>::None).unwrap();
    let (_, second) = registry.register("second", Option::<BlobTail>::None).unwrap();

    assert!(registry.get("first").is_some());
    assert!(registry.get("second").is_some());

    drop((first, second));
    assert!(registry.get("first").is_none());
    assert!(registry.get("second").is_none());
}

#[test]
fn test_same_digest_returns_the_registered_handle() {
    let registry = DownloadRegistry::default();
    let (mut handle, producer) = registry.register("digest", Option::<BlobTail>::None).unwrap();
    let mut existing = registry.register("digest", Option::<BlobTail>::None).unwrap_err();

    assert!(handle.progress().same_channel(existing.progress()));

    drop(producer);
}

#[test]
fn test_finish_removes_and_notifies() {
    let registry = DownloadRegistry::default();
    let (mut handle, producer) = registry.register("digest", Option::<BlobTail>::None).unwrap();

    producer.finish(Ok(()));

    assert!(registry.get("digest").is_none());
    assert_eq!(handle.progress().borrow_and_update().done.clone(), Some(Ok(())));
}

#[test]
fn test_cancellation_removes_and_notifies() {
    let registry = DownloadRegistry::default();
    let (mut handle, producer) = registry.register("digest", Option::<BlobTail>::None).unwrap();

    drop(producer);

    assert!(registry.get("digest").is_none());
    assert_eq!(
        handle.progress().borrow_and_update().done.clone(),
        Some(Err("blob transfer abandoned".to_owned()))
    );
}

#[test]
fn test_finished_producer_releases_digest_for_replacement() {
    let registry = DownloadRegistry::default();
    let (mut old_handle, old) = registry.register("digest", Option::<BlobTail>::None).unwrap();

    old.finish(Ok(()));
    let (mut replacement, current) = registry.register("digest", Option::<BlobTail>::None).unwrap();

    let mut registered = registry.get("digest").unwrap();
    assert!(registered.progress().same_channel(replacement.progress()));
    assert_eq!(old_handle.progress().borrow_and_update().done.clone(), Some(Ok(())));
    drop(current);
}

#[test]
fn test_progress_tracks_flushed_bytes() {
    let registry = DownloadRegistry::default();
    let (mut handle, producer) = registry.register("digest", Option::<BlobTail>::None).unwrap();

    assert!(handle.tail().is_none());
    assert_eq!(producer.flushed(), 0);
    producer.publish_flushed(41);
    assert_eq!(producer.flushed(), 41);
    assert_eq!(handle.progress().borrow_and_update().flushed, 41);
}
