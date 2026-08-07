+++
title = "Policy settings"
description = "OCI repository size, identity, and quota enforcement."
weight = 35
+++

OCI applies the common `[index.policy]` name and size rules to repository names, blobs, and manifests. Hosted pushes
also enforce repository quotas:

Policy and access globs match the lowercase repository path as written. `*` crosses `/`, so `team/*` covers the full
subtree below `team/`.

| Key                        | Meaning                                           |
| -------------------------- | ------------------------------------------------- |
| `max_file_size_bytes`      | Largest accepted blob or manifest                 |
| `max_accounted_bytes`      | Deduplicated bytes charged to one repository      |
| `max_projects`             | Distinct repository identities                    |
| `max_versions_per_project` | Tags retained for one repository                  |
| `quota_audit`              | Record a would-reject decision and admit the push |

A blob upload, cross-repository mount, or manifest push reserves capacity before it becomes discoverable. Enforcement
returns `403 DENIED` when a reservation would cross a limit. A digest is charged once per repository. Leaving every
quota unset disables quota accounting.

See [repository quotas](@/core/quotas.md) for reservation and transaction semantics.

## Preview decisions

`peryx policy dry-run --index images --project team/api` scans cached and hosted OCI records without fetching an
upstream or changing the served index. It prints tab-separated denial rows for the selected repository.

## Push quotas

Blob uploads and cross-repository mounts reserve layer bytes. Manifest publication reserves the manifest document and,
for a tagged push, one version. A repository is charged once per digest, so a repeated push, a mount of a present blob,
and concurrent uploads of one digest do not duplicate accounted bytes.

A denial returns distribution-spec code `DENIED` with `403 Forbidden` and publishes no repository membership, manifest,
or tag. Digest mismatch and storage failures release their reservation. Decisions increment the `quota_admitted` and
`quota_rejected` metric families without repository or project labels.
