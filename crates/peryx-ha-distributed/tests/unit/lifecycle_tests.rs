use super::lifecycle::Lifecycle;

#[tokio::test]
async fn activation_releases_all_waiters() {
    let (lifecycle, _) = Lifecycle::new();
    let first = tokio::spawn({
        let lifecycle = lifecycle.clone();
        async move { lifecycle.activated().await }
    });
    let second = tokio::spawn({
        let lifecycle = lifecycle.clone();
        async move { lifecycle.activated().await }
    });

    lifecycle.activate();

    assert!(first.await.unwrap());
    assert!(second.await.unwrap());
}

#[tokio::test]
async fn cancellation_rejects_activation() {
    let (lifecycle, _) = Lifecycle::new();
    lifecycle.cancel();
    assert!(!lifecycle.activated().await);
}

#[tokio::test]
async fn supervision_keeps_the_first_failure() {
    let (lifecycle, mut failures) = Lifecycle::new();
    lifecycle.fail("replica failed");
    lifecycle.fail("listener failed");
    assert_eq!(failures.wait().await, "replica failed");
}

#[tokio::test]
async fn cancellation_discards_late_failures() {
    let (lifecycle, mut failures) = Lifecycle::new();
    lifecycle.cancel();
    lifecycle.fail("shutdown");
    drop(lifecycle);
    assert_eq!(failures.wait().await, "distributed availability supervision stopped");
}
