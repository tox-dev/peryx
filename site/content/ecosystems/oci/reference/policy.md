+++
title = "Policy settings"
description = "OCI repository size, identity, and quota enforcement."
weight = 35
+++

OCI enforces `allow_resources`, `block_resources`, `protected_resources`, and `max_artifact_size_bytes`. Hosted pushes
also enforce `max_accounted_bytes`, `max_resources`, `quota_audit`, and the OCI-specific `max_tags_per_repository`.
Configuration accepts `max_resource_size_bytes`, but OCI does not enforce that limit.

Policy resource values match the lowercase repository path as written. Allow and block lists match exact paths;
protected resources may end in `*` to match a prefix. Access globs use `*` for any run of characters, including `/`.

| Key                       | Meaning                                                        |
| ------------------------- | -------------------------------------------------------------- |
| `allow_resources`         | Permit serving or mirroring only for listed repositories       |
| `block_resources`         | Deny listed repositories                                       |
| `protected_resources`     | Prevent exact or prefix-matched repositories from falling back |
| `max_artifact_size_bytes` | Cap one blob or manifest                                       |
| `max_accounted_bytes`     | Deduplicated bytes charged to one repository                   |
| `max_resources`           | Distinct repository identities                                 |
| `max_tags_per_repository` | Tags retained for one repository                               |
| `quota_audit`             | Record a would-reject quota decision and admit the push        |

Every write that adds content admits on the name lists first. A manifest push, a monolithic or resumable blob upload, a
cross-repository mount, and a restore from the trash all answer `403 DENIED` for a blocked repository, whether or not a
size or quota limit is configured. Deleting from a blocked repository stays permitted, so an operator can reclaim what
it already holds.

A blob upload, cross-repository mount, or manifest push reserves capacity before it becomes discoverable. Enforcement
returns `403 DENIED` when a reservation would cross a limit. Each repository accounts for a digest once. Leaving every
quota unset disables quota accounting.

See [repository quotas](@/core/repositories/quotas.md) for reservation and transaction semantics.

## Push quotas

Blob uploads and cross-repository mounts reserve layer bytes. Manifest publication reserves the manifest document and,
for a tagged push, one version. Accounting counts each digest once per repository, so a repeated push, a mount of a
present blob, and concurrent uploads of one digest do not duplicate accounted bytes.

A denial returns distribution-spec code `DENIED` with `403 Forbidden` and publishes no repository membership, manifest,
or tag. Digest mismatch and storage failures release their reservation. Decisions increment the `quota_admitted` and
`quota_rejected` metric families without repository or project labels.
