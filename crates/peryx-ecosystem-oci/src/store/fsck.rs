use std::collections::BTreeSet;
use std::io::Write;

use peryx_storage::blob::{BlobStorage, Digest};
use peryx_storage::meta::MetaStore;

use super::{MANIFEST_PREFIX, Manifest, TAG_PREFIX, blob_digest, manifest_descriptors};

pub fn fsck_metadata(meta: &MetaStore, blobs: &BlobStorage, out: &mut dyn Write) -> Result<u64, String> {
    let manifests = meta
        .read_driver_txn(|txn| txn.prefix(MANIFEST_PREFIX))
        .map_err(|error| error.to_string())?;
    let manifest_digests = manifests
        .iter()
        .filter_map(|(key, raw)| Manifest::decode(raw).map(|_| key[MANIFEST_PREFIX.len()..].to_owned()))
        .collect::<BTreeSet<_>>();
    let mut problems = 0_u64;
    for (key, raw) in manifests {
        let digest = &key[MANIFEST_PREFIX.len()..];
        let Some(manifest) = Manifest::decode(&raw) else {
            report(out, "manifest", digest, "invalid record")?;
            problems += 1;
            continue;
        };
        let canonical = format!("sha256:{}", Digest::of(&manifest.bytes).as_str());
        if digest != canonical {
            report(out, "manifest", digest, "digest mismatch")?;
            problems += 1;
        }
        if serde_json::from_slice::<serde_json::Value>(&manifest.bytes).is_err() {
            report(out, "manifest", digest, "invalid document")?;
            problems += 1;
            continue;
        }
        let (children, descriptor_blobs) = manifest_descriptors(&manifest.bytes);
        for child in children {
            let reason = if blob_digest(&child).is_none() {
                Some(format!("invalid child manifest {child}"))
            } else if !manifest_digests.contains(&child) {
                Some(format!("missing child manifest {child}"))
            } else {
                None
            };
            if let Some(reason) = reason {
                report(out, "descriptor", digest, &reason)?;
                problems += 1;
            }
        }
        for descriptor in descriptor_blobs {
            let Some(storage) = blob_digest(&descriptor) else {
                report(out, "descriptor", digest, &format!("invalid blob {descriptor}"))?;
                problems += 1;
                continue;
            };
            if blobs
                .blocking()
                .head(&storage)
                .map_err(|error| error.to_string())?
                .is_none()
            {
                report(out, "descriptor", digest, &format!("missing blob {descriptor}"))?;
                problems += 1;
            }
        }
    }
    for (key, raw) in meta
        .read_driver_txn(|txn| txn.prefix(TAG_PREFIX))
        .map_err(|error| error.to_string())?
    {
        let subject = key[TAG_PREFIX.len()..].replace('\0', "/");
        let Ok(target) = std::str::from_utf8(&raw) else {
            report(out, "tag", &subject, "invalid record")?;
            problems += 1;
            continue;
        };
        if blob_digest(target).is_none() {
            report(out, "tag", &subject, "invalid manifest digest")?;
            problems += 1;
        } else if !manifest_digests.contains(target) {
            report(out, "tag", &subject, &format!("missing manifest {target}"))?;
            problems += 1;
        }
    }
    Ok(problems)
}

fn report(out: &mut dyn Write, kind: &str, subject: &str, reason: &str) -> Result<(), String> {
    writeln!(out, "metadata\toci\t{kind}\t{subject}\t{reason}").map_err(|error| error.to_string())
}
