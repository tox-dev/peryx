+++
title = "How peryx scopes and serves manifest reads"
description = "Why a by-digest manifest pull is authorized against the requesting repository, not the shared content store, and why an index tag can hand back a single-platform image."
weight = 4
+++

peryx stores every manifest once, addressed by the sha256 of its bytes, in a global content pool the OCI indexes share.
That is what makes deduplication work: the same image pulled through two repositories keeps one copy. It also raises a
question the read path has to answer on every by-digest pull, which repository is allowed to read those shared bytes,
and a compatibility question a tag pull has to answer, which of a multi-platform image's parts to hand a client that
cannot read the index.

## Why a by-digest read is scoped to the repository

A `GET` or `HEAD /v2/<name>/manifests/<digest>` names a manifest by its content address. peryx used to answer it from
the shared pool under any repository the caller could address, checking only that repository's policy, never whether the
repository held the digest. So the bytes came back as long as the digest existed anywhere in the store.

Digests are not secret. They appear in tags, referrers, the catalog, image indexes, CI logs, and every `docker pull`
output. A caller who learned one, from a colleague's build log or a public base image, could pull or probe a private
image's manifest by digest under any repository name it was allowed to read, across index and tenant boundaries. The
by-digest read was the leak; [#103](https://github.com/tox-dev/peryx/issues/103) had already closed the same gap on
`DELETE` and left the read open, so this ([#136](https://github.com/tox-dev/peryx/issues/136)) is its read half.

peryx now authorizes a by-digest read against per-repository membership rather than the digest's presence in the pool. A
repository reads a digest by digest only when one of its serving members recorded serving that digest under the
repository:

- a manifest pushed, pulled, tagged, or mirrored under that member and repository,
- a child of an image index or manifest list the member stores, or
- a referrer pushed there.

Blobs already gate this way; manifests now match. A proxy member still pulls an unauthorized miss through its upstream,
scoped to the requested repository, so a legitimate pull-through of an image the repository serves stays intact, as do
referrer and image-index child pulls. A digest that no member records and no proxy can fetch returns the ordinary
`404 MANIFEST_UNKNOWN`, the same answer as a digest that does not exist, so the response never reveals that the digest
is stored elsewhere.

The membership record is written wherever peryx stores a manifest: its own digest, plus each child an image index or
manifest list names. A by-digest delete drops the record unless an index the repository still serves names the digest as
a child, so deleting an image and re-pushing it under another repository cannot revive a stale grant.

## Blob bytes and repository links stay separate

peryx stores one copy of each blob in the content-addressed store and writes an `(index, repository, digest)` link for
each repository that serves it. peryx records the link after an upload or proxy fetch. A manifest write records links
for its config and layers, so a mirrored or cached manifest can serve its descriptors without copying bytes.

peryx checks the repository link before it uses cached bytes. If the cache contains the digest through another
repository, peryx sends a repository-scoped upstream `HEAD`, records the link after a `2xx` response, and reuses the
bytes. peryx returns `404 BLOB_UNKNOWN` when the target repository lacks the digest.

peryx requires the source repository name and pull authorization for a cross-repository mount, then copies the source
link to the target. For a delete, peryx removes the target link and leaves the shared content store unchanged.
`cache purge orphaned-blobs` reclaims the payload after no installed ecosystem references it. The collector checks
references a second time after its disk walk, so the collector preserves bytes that a concurrent publication references.

## Why an index tag can serve a single-platform image

A tag often points at an image index (an OCI index or a Docker manifest list), the small document that maps each
platform, `linux/amd64`, `linux/arm64`, to the per-platform image manifest for it. A modern client pulls the index,
picks the entry for its platform, and pulls that child.

Docker below 17.06 predates the manifest list. It sends an `Accept` naming only the schema-2 image manifest and cannot
parse an index, so a registry that hands it the index body on a tag pull gives it something it cannot read. peryx now
negotiates the manifest read against `Accept` the way an upstream registry does: when a tag resolves to an index or
manifest list and the client's `Accept` names neither list media type, peryx serves the index's `linux/amd64` child
image manifest, reading it from the store or fetching it by digest through a proxy member, with the child's
`Content-Type` and `Docker-Content-Digest`. A `HEAD` returns the same headers with an empty body.

Nothing else changes. An `Accept` that names a list type, or is absent, still gets the index, as does an index with no
`linux/amd64` child; only the serve path negotiates, and a push stores what it is given. Modern docker, podman,
containerd, and oras all send `Accept` lists that name the index types, so they receive the index and never see the
substitution ([#114](https://github.com/tox-dev/peryx/issues/114)).

## In practice

- The manifest routes and their status codes: [HTTP endpoints](@/ecosystems/oci/reference/endpoints.md)
- How a repository shadows an upstream image: [the index model](@/core/indexes.md)
- What a digest addresses and how peryx verifies it: [OCI](@/ecosystems/oci/_index.md)
