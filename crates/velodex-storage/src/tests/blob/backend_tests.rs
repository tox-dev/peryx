use super::store;

#[test]
fn test_blob_backend_trait_round_trips() {
    use crate::blob::BlobBackend;
    fn exercise(store: &impl BlobBackend) {
        let digest = store.write(b"pkg").unwrap();
        assert!(store.exists(&digest));
        assert_eq!(store.read(&digest).unwrap(), b"pkg");
        assert!(store.verify(&digest).unwrap());
        store.write_verified(b"pkg", &digest).unwrap();
        assert!(store.remove(&digest).unwrap());
        assert!(!store.exists(&digest));
    }
    let (_dir, store) = store();
    exercise(&store);
}
