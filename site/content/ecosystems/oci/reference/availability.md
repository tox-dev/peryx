+++
title = "Availability behavior"
description = "OCI authority keys, distribution retries, readable frontiers, reclamation references, and log mapping."
weight = 8
+++

Distribution routes use the shared [availability contracts](@/core/availability-contracts.md).

- A read-only replica refuses blob and manifest mutations with `503 Service Unavailable` before protocol dispatch.
- A mutation under a superseded authority returns `503 Service Unavailable` with code `UNAVAILABLE`. Retry it against
  the current writer.
- Blob and manifest retries converge by digest. A conflicting tag mutation remains a conflict.
- Mutable tag reads remain behind the readable frontier. Digest reads require verified bytes.
- Cross-datacenter placement tracks each manifest and referenced blob as separate content.

Availability mode does not change the distribution protocol. It changes the acknowledgement point.

## Authority keys

An OCI authority is the repository path under the `oci:` scheme prefix. Repository `library/nginx` uses
`oci:library/nginx`. The driver preserves distinct paths. The prefix separates OCI keys from unprefixed PyPI project
keys.

The first manifest publication assigns the repository home. Blob placement remains content-addressed and can span
repositories and datacenters.

## Finalization and retries

Blob membership, manifest publication, tag replacement, and delete operations fence against the committed repository
epoch. A resumable upload retains its session and staged bytes after a stale-epoch refusal, so the client can retry
finalization without sending the blob again. A monolithic upload retries its request.

The response omits leader, datacenter, and peer addresses. See
[Registry behavior](@/ecosystems/oci/registry-behavior.md) for upload-state details.

## Derived views

Hosted tag, manifest, and referrer reads remain behind the readable frontier while the required search view trails the
metadata serial. A virtual repository also waits for hosted members. Cached repositories report upstream state and have
no local hosted serial to gate.

## Remote content and reclamation {#reclamation-references}

A blob that misses local storage can use [remote read-through](@/core/remote-read-through.md) from a verified peer
placement.

The OCI reclamation inventory retains repository blob memberships plus each manifest's config, layers, and child content
descriptors. Trash and verified placements add the shared references described in
[Blob reclamation](@/core/availability-blob-reclamation.md).

## Logging

Availability traces map manifest or repository publication to `publish`, deletion to `delete`, upstream content
population to `cache-fill`, and metadata visibility changes to `visibility`. Content details stay out of the operation
envelope event.

See [Client behavior across availability modes](@/core/availability-client-behavior.md) and [Logging](@/core/logging.md)
for shared fields.
