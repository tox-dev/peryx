+++
title = "Registry behavior decisions"
description = "Digest handling, upload-session lifetime, and write fencing at OCI protocol boundaries."
weight = 5
aliases = [ "/ecosystems/oci/content-digest-algorithms/", "/ecosystems/oci/upload-conformance/"]
+++

The OCI implementation accepts Distribution requests, translates them to repository mutations, and reports failures with
OCI status codes and error envelopes. This page records decisions that are easy to miss when reading an endpoint table.

## Digest handling

Peryx stores manifests byte-for-byte under their computed sha256 digest. It verifies an upstream `Docker-Content-Digest`
when the upstream advertises sha256. Other valid algorithms remain usable at the protocol boundary because comparing
digests from different algorithms proves nothing. Blob uploads remain sha256-only.

A manifest or blob digest does not grant access by itself. The requested repository must record membership for that
digest, or a cached member must fetch it from that repository upstream. This prevents a digest learned elsewhere from
exposing content through another repository name.

The referrers route validates its subject with the OCI digest grammar before lookup. A malformed subject returns
`400 DIGEST_INVALID`; a valid subject with no referrers returns an empty index.

## Upload sessions

A random session identifier locates a record in the metadata store and staged bytes on the filesystem. The opening
repository stays bound to the session, and each follow-up request repeats authorization. After a restart, peryx reads
both parts, and a client may query the recorded offset, append the next range, finish with a matching sha256 digest, or
cancel with `DELETE`.

An unfinished session remains until `DELETE`, a size rejection, or idle reclamation after one hour without activity. The
local maintenance worker checks once per minute by default. Digest and range mismatches keep the session and staged
bytes; a client can retry from the recorded offset.

## Write fencing

With `availability.mode = "none"`, OCI writes commit locally and use no distributed authority state. The `dc` and `ha`
modes pass hosted mutations through the configured authority epoch before committing repository metadata. A stale writer
receives `503 UNAVAILABLE` and may retry after topology converges.

The fence applies to manifest publication and deletion, blob finalization and deletion, cross-repository mounts, and
restore. Cache fills remain derived state and do not enter the authoritative mutation journal.

## Durability acknowledgement

The Distribution specification fixes which status code a push answers; it says nothing about how many copies stand
behind it. `[availability.write_ack]` does, and a blob push answers `201 Created` only after that policy passes. A
monolithic `PUT`, a resumable finalize, and a cross-repository mount all resolve through the same acknowledgement the
PyPI upload path uses, so a `201` here and a `200` there mean the same thing about how much of the cluster holds the
content.

Under `availability.mode = "none"` the local commit is the whole policy and pushes answer exactly as before. Under `dc`
the policy counts same-datacenter member receipts for the blob; under `ha` it also waits for the membership write to
apply in the share of remote datacenters the policy names.

A push still short of its policy when `write_ack.deadline-secs` elapses answers `503 UNAVAILABLE` with `Retry-After`
rather than a success code, because the durable completion may land after the client stops waiting. The content and its
repository membership are committed either way, which is what gives the metadata frontier something to acknowledge, so
what the deadline withholds is the promise, not the write. The push records a pending operation keyed by index,
repository, and digest; the client resends the identical request, and the content-addressed commit and the membership
upsert replay onto the same operation without a second effect.

Manifest publication is a metadata-only write with no blob bytes and does not yet take part in this acknowledgement;
that gap needs a metadata-only durability contract in the shared availability crates.

## Details

- [Registry behavior reference](@/ecosystems/oci/reference/registry-behavior.md) lists exact headers, status codes, and
  digest rules.
- [Work with registry behavior](@/ecosystems/oci/guides/registry-behavior.md) contains cancellation, resume, and
  reclamation commands.
- [Chunked upload tutorial](@/ecosystems/oci/tutorials/chunked-upload.md) exercises the upload protocol with `curl`.
- [Availability behavior](@/ecosystems/oci/reference/availability.md) covers routing and retry behavior for `dc` and
  `ha`.
