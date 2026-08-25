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

A random session identifier locates process-local staged bytes. The opening repository stays bound to the session, and
each follow-up request repeats authorization. A client may query the current offset, append the next range, finish with
a matching sha256 digest, or cancel with `DELETE`.

Peryx expires a session after one hour without activity. Cancellation, a digest mismatch, and a file-size policy denial
remove its staged bytes. A range mismatch returns `416` with the committed offset so the client can resume without
restarting.

## Write fencing

With `availability.mode = "none"`, OCI writes commit locally and use no distributed authority state. The `dc` and `ha`
modes pass hosted mutations through the configured authority epoch before committing repository metadata. A stale writer
receives `503 UNAVAILABLE` and may retry after topology converges.

The fence applies to manifest publication and deletion, blob finalization and deletion, cross-repository mounts, and
restore. Cache fills remain derived state and do not enter the authoritative mutation journal.

## Details

- [Registry behavior reference](@/ecosystems/oci/reference/registry-behavior.md) lists exact headers, status codes, and
  digest rules.
- [Work with registry behavior](@/ecosystems/oci/guides/registry-behavior.md) contains cancellation, resume, and
  reclamation commands.
- [Chunked upload tutorial](@/ecosystems/oci/tutorials/chunked-upload.md) exercises the upload protocol with `curl`.
- [Availability behavior](@/ecosystems/oci/reference/availability.md) covers routing and retry behavior for `dc` and
  `ha`.
