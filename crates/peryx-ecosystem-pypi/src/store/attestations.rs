use std::collections::BTreeMap;

use peryx_storage::meta::{DriverTxn, MetaError, MetaStore};

use super::{
    UpstreamAttestation, project_attestation_live_key, project_attestation_live_prefix,
    project_generation_attestation_key, project_generation_attestation_prefix, upstream_attestation_key,
    upstream_attestation_prefix,
};

fn register_upstream_attestation_in_txn(
    txn: &mut DriverTxn<'_>,
    index: &str,
    artifact_sha256: &str,
    filename: &str,
    record: &UpstreamAttestation,
) -> Result<String, MetaError> {
    let key = upstream_attestation_key(index, artifact_sha256, filename, &record.project);
    txn.get(&key)
        .and_then(|raw| {
            raw.map(|raw| serde_json::from_slice::<UpstreamAttestation>(&raw).map_err(MetaError::from))
                .transpose()
        })
        .and_then(|current| {
            if current.is_some_and(|current| {
                current.url == record.url
                    && current.source == record.source
                    && current.project == record.project
                    && current.upstream == record.upstream
            }) {
                return Ok(key);
            }
            serde_json::to_vec(record)
                .map_err(MetaError::from)
                .and_then(|encoded| txn.put_local(&key, &encoded))
                .map(|()| key)
        })
}

pub(super) fn replace_project_upstream_attestations_in_txn(
    txn: &mut DriverTxn<'_>,
    index: &str,
    project: &str,
    upstream: Option<&str>,
    attestations: &[(String, String, String)],
) -> Result<(), MetaError> {
    let desired: BTreeMap<_, _> = attestations
        .iter()
        .map(|(digest, filename, url)| {
            (
                project_attestation_live_key(index, project, digest, filename),
                (
                    digest,
                    filename,
                    UpstreamAttestation::remote(url, index, project, upstream),
                ),
            )
        })
        .collect();
    txn.prefix(&project_attestation_live_prefix(index, project))
        .and_then(|current| {
            current.into_iter().try_for_each(|(owner_key, main_key)| {
                if desired.contains_key(&owner_key) {
                    return Ok(());
                }
                serde_json::from_slice::<String>(&main_key)
                    .map_err(MetaError::from)
                    .and_then(|main_key| txn.remove(&main_key).map(|_| ()))
                    .and_then(|()| txn.remove(&owner_key).map(|_| ()))
            })
        })
        .and_then(|()| {
            desired
                .into_iter()
                .try_for_each(|(owner_key, (digest, filename, record))| {
                    register_upstream_attestation_in_txn(txn, index, digest, filename, &record).and_then(|main_key| {
                        serde_json::to_vec(&main_key)
                            .map_err(MetaError::from)
                            .and_then(|encoded| txn.put_local(&owner_key, &encoded))
                    })
                })
        })
}

pub(super) fn stage_upstream_attestation_in_txn(
    txn: &mut DriverTxn<'_>,
    index: &str,
    generation: u64,
    artifact_sha256: &str,
    filename: &str,
    record: &UpstreamAttestation,
) -> Result<(), MetaError> {
    let key = project_generation_attestation_key(index, &record.project, generation, artifact_sha256, filename);
    serde_json::to_vec(&(artifact_sha256, filename, record))
        .map_err(MetaError::from)
        .and_then(|encoded| txn.put_local(&key, &encoded))
}

pub(super) fn publish_staged_upstream_attestations_in_txn(
    txn: &mut DriverTxn<'_>,
    index: &str,
    project: &str,
    generation: u64,
) -> Result<(), MetaError> {
    let prefix = project_generation_attestation_prefix(index, project, generation);
    txn.prefix(&prefix).and_then(|staged| {
        staged
            .iter()
            .map(|(_, raw)| serde_json::from_slice::<(String, String, UpstreamAttestation)>(raw))
            .collect::<Result<Vec<_>, _>>()
            .map_err(MetaError::from)
            .and_then(|records| {
                let upstream = records.last().and_then(|(_, _, record)| record.upstream.clone());
                let attestations = records
                    .into_iter()
                    .map(|(digest, filename, record)| (digest, filename, record.url))
                    .collect::<Vec<_>>();
                replace_project_upstream_attestations_in_txn(txn, index, project, upstream.as_deref(), &attestations)
            })
            .and_then(|()| staged.into_iter().try_for_each(|(key, _)| txn.remove(&key).map(|_| ())))
    })
}

pub(super) fn list_upstream_attestations(
    meta: &MetaStore,
    index: &str,
    artifact_sha256: &str,
    filename: &str,
) -> Result<Vec<UpstreamAttestation>, MetaError> {
    let mut records = Vec::new();
    let prefix = upstream_attestation_prefix(index, artifact_sha256, filename);
    meta.visit_driver_prefix(&prefix, |_key, raw| records.push(raw.to_vec()))
        .map(|()| records)
        .and_then(|records| {
            records
                .into_iter()
                .map(|raw| serde_json::from_slice(&raw).map_err(MetaError::from))
                .collect()
        })
}
