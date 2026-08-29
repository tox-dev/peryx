use crate::meta::{AnalyticsCheckpoint, fault};

#[test]
fn test_analytics_checkpoint_recovers_storage_failures_without_mixed_snapshots() {
    let mut failures = 0;
    for fail_after in 0..256 {
        let (store, inner, fault) = fault::initialized();
        store
            .analytics()
            .save_checkpoint(b"old lifetime", b"old daily")
            .unwrap();
        drop(store);
        let store = fault::reopen(&inner, &fault);
        fault.arm(fail_after);
        if store
            .analytics()
            .save_checkpoint(b"new lifetime", b"new daily")
            .is_err()
        {
            failures += 1;
            fault.disable();
            drop(store);
            let store = fault::reopen(&inner, &fault);
            let observed = store.analytics().load_checkpoint().unwrap();
            let new_checkpoint = checkpoint(b"new lifetime", b"new daily");
            assert!([checkpoint(b"old lifetime", b"old daily"), new_checkpoint.clone()].contains(&observed));
            store
                .analytics()
                .save_checkpoint(b"new lifetime", b"new daily")
                .unwrap();
            drop(store);
            assert_eq!(
                fault::reopen(&inner, &fault).analytics().load_checkpoint().unwrap(),
                new_checkpoint
            );
        }
    }
    assert!(failures > 0);
}

fn checkpoint(lifetime: &[u8], daily: &[u8]) -> AnalyticsCheckpoint {
    AnalyticsCheckpoint {
        lifetime: Some(lifetime.to_vec()),
        daily: Some(daily.to_vec()),
    }
}
