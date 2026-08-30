+++
title = "Manifest read scope and platform selection"
description = "Repository-scoped digest reads and single-platform responses for index tags."
weight = 4
+++

peryx stores one copy of each manifest, addressed by the sha256 of its bytes, in a content pool shared by OCI indexes. A
digest read still requires membership in the requested repository. A tag read can select one manifest from a
multi-platform index for clients that cannot read the index media type.

## Repository-scoped digest reads

A `GET` or `HEAD /v2/<name>/manifests/<digest>` names a manifest by its content address. peryx used to answer it from
the shared pool under any repository the caller could address. It checked repository policy but not digest membership,
so a digest stored for another repository could be returned.

Digests are not secret. They appear in tags, referrers, the catalog, image indexes, CI logs, and every `docker pull`
output. A caller who learned one, from a colleague's build log or a public base image, could pull or probe a private
image's manifest by digest under any repository name it was allowed to read, across index and tenant boundaries. Issue
[#103](https://github.com/tox-dev/peryx/issues/103) added the membership check to `DELETE`; issue
[#136](https://github.com/tox-dev/peryx/issues/136) applied it to reads.

peryx now authorizes a by-digest read against per-repository membership rather than the digest's presence in the pool. A
repository reads a digest by digest only when one of its serving members recorded serving that digest under the
repository:

- a manifest pushed, pulled, tagged, or mirrored under that member and repository,
- a child of an image index or manifest list the member stores, or
- a referrer pushed there.

Blobs already gate this way; manifests now match. A proxy member still pulls an unauthorized miss through its upstream,
scoped to the requested repository, so pull-through, referrer, and image-index child reads remain available. A digest
that no member records and no proxy can fetch returns `404 MANIFEST_UNKNOWN`, the same response as an unknown digest.
The response does not disclose storage membership in another repository.

Content negotiation resolves an index's `linux/amd64` child through that same by-digest path, so a client that accepts
only the schema-2 image type receives the substituted child under the requesting repository's membership rather than
whatever copy the shared pool happens to hold.

The membership record is written wherever peryx stores a manifest: its own digest, plus each child an image index or
manifest list names. A hosted push rejects an index whose child is not already a member of the target repository, so
recording the index grants nothing the pusher could not already read. A by-digest delete keeps that record and adds a
repository tombstone. The tombstone blocks reads while the membership retains the scope needed for restore; another
repository's push cannot expose the deleted digest.

## Blob storage and repository links

peryx stores one copy of each blob in the content-addressed store and writes an `(index, repository, digest)` link for
each repository that serves it. peryx records the link after an upload or proxy fetch. A manifest write records links
for its config and layers, so a mirrored or cached manifest can serve its descriptors without copying bytes.

peryx checks the repository link before it uses cached bytes. If the cache contains the digest through another
repository, peryx sends a repository-scoped upstream `HEAD`, records the link after a `2xx` response, and reuses the
bytes. peryx returns `404 BLOB_UNKNOWN` when the target repository lacks the digest.

peryx requires the source repository name and pull authorization for a cross-repository mount, then copies the source
link to the target. For a delete, peryx removes the target link and leaves the shared content store unchanged.
`cache purge orphaned-blobs` reclaims the payload after no registered blob-reference provider reports it. The collector
checks again after its disk walk, preserving bytes referenced by a concurrent publication.

## Single-platform tag responses

A tag often points at an image index (an OCI index or a Docker manifest list), the small document that maps each
platform, `linux/amd64`, `linux/arm64`, to the per-platform image manifest for it. A modern client pulls the index,
picks the entry for its platform, and pulls that child.

Docker below 17.06 predates the manifest list. It sends an `Accept` naming only the schema-2 image manifest and cannot
parse an index, so a registry that hands it the index body on a tag pull gives it something it cannot read. peryx
negotiates the manifest read against `Accept` the way HTTP content negotiation prescribes, the reference the OCI
distribution spec defers to: it reads every `Accept` field line as one combined media-range list, per RFC 9110, and
computes the served list type's effective quality. The most specific matching range decides it, an exact type over a
`type/*` over `*/*`, so `application/*` and `*/*` accept the index while a `q=0` on the matching range rejects it even
when a broader range would accept. Only when the list type has no positive quality does peryx serve the index's
`linux/amd64` child image manifest, reading it from the store or fetching it by digest through a proxy member, with the
child's `Content-Type` and `Docker-Content-Digest`. A `HEAD` returns the same headers with an empty body.

An `Accept` that gives a list type positive quality, that is absent, empty, or a wildcard (`*/*` or `*`, curl's default
and what many HTTP clients send), still gets the index, as does an index with no `linux/amd64` child; only a client
whose media ranges leave the list type unacceptable gets the substitution. A push stores what it is given. Modern
docker, podman, containerd, and oras all send `Accept` lists that name the index types, so they receive the index
([#114](https://github.com/tox-dev/peryx/issues/114)). Because the same tag can return the index or its child depending
on `Accept`, the serve carries `Vary: Accept` so a shared cache keys on it.

## Operational checks

- The manifest routes and their status codes: [HTTP endpoints](@/ecosystems/oci/reference/endpoints.md)
- How a repository shadows an upstream image: [the index model](@/core/repositories/indexes.md)
- Digest addressing and verification: [OCI](@/ecosystems/oci/_index.md)
