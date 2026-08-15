use std::collections::BTreeSet;
use std::sync::Arc;

use peryx_core::{Ecosystem, TrashRecord};
use peryx_driver::DriverSet;
use peryx_driver::serving::{BlobReferenceDriver, CapabilityRegistrar, EcosystemDriver, TrashDriver};
use peryx_storage::meta::MetaStore;

#[derive(Clone, Copy)]
enum Capability {
    Absent,
    Ready,
    Failing,
}

struct References {
    blobs: Capability,
    trash: Capability,
}

impl EcosystemDriver for References {
    fn ecosystem(&self) -> Ecosystem {
        Ecosystem::new("example")
    }
}

impl BlobReferenceDriver for References {
    fn referenced_blob_digests(&self, _: &MetaStore) -> Result<BTreeSet<String>, String> {
        if matches!(self.blobs, Capability::Failing) {
            Err("references unavailable".to_owned())
        } else {
            Ok(BTreeSet::from(["base".to_owned()]))
        }
    }
}

impl TrashDriver for References {
    fn trash_records(&self, _: &MetaStore, indexes: &[String]) -> Result<Vec<TrashRecord>, String> {
        if matches!(self.trash, Capability::Failing) {
            return Err("trash unavailable".to_owned());
        }
        Ok(indexes
            .iter()
            .map(|repository| TrashRecord {
                ecosystem: Ecosystem::new("example"),
                repository: repository.as_str().into(),
                resource: "resource".into(),
                artifact: None,
                digest: Some("sha256:trash".to_owned()),
                reason: None,
                actor: None,
                deleted_at_unix: 0,
                retained: true,
            })
            .collect())
    }
}

fn store() -> (tempfile::TempDir, MetaStore) {
    let directory = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(directory.path().join("peryx.redb")).unwrap();
    (directory, meta)
}

fn drivers(references: References) -> DriverSet {
    let blobs = references.blobs;
    let trash = references.trash;
    let references = Arc::new(references);
    let mut drivers = DriverSet::default().with(references.clone());
    if !matches!(blobs, Capability::Absent) {
        drivers.register_blob_references(Ecosystem::new("example"), references.clone());
    }
    if !matches!(trash, Capability::Absent) {
        drivers.register_trash(Ecosystem::new("example"), references);
    }
    drivers
}

#[test]
fn reference_inventory_merges_live_and_trash_digests() {
    let (_directory, meta) = store();
    let inventory = super::reference_inventory(
        drivers(References {
            blobs: Capability::Ready,
            trash: Capability::Ready,
        }),
        meta,
        vec!["main".to_owned()],
    );

    assert_eq!(
        inventory.referenced().unwrap(),
        BTreeSet::from(["base".to_owned(), "trash".to_owned()])
    );
}

#[test]
fn reference_inventory_propagates_driver_failures() {
    let (_directory, meta) = store();
    let inventory = super::reference_inventory(
        drivers(References {
            blobs: Capability::Failing,
            trash: Capability::Ready,
        }),
        meta,
        Vec::new(),
    );

    assert_eq!(inventory.referenced(), Err("references unavailable".to_owned()));
}

#[test]
fn reference_inventory_propagates_trash_failures() {
    let (_directory, meta) = store();
    let inventory = super::reference_inventory(
        drivers(References {
            blobs: Capability::Ready,
            trash: Capability::Failing,
        }),
        meta,
        vec!["main".to_owned()],
    );

    assert_eq!(
        inventory.referenced(),
        Err("scan example trash: trash unavailable".to_owned())
    );
}

#[test]
fn reference_inventory_skips_absent_capabilities() {
    let (_directory, meta) = store();
    let inventory = super::reference_inventory(
        drivers(References {
            blobs: Capability::Absent,
            trash: Capability::Absent,
        }),
        meta,
        vec!["main".to_owned()],
    );

    assert_eq!(inventory.referenced().unwrap(), BTreeSet::new());
}
