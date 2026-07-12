+++
title = "Why peryx validates up front and reclaims dead uploads"
description = "Why the referrers API rejects a malformed digest instead of answering an empty 200, why cancelling an upload unlinks its staged file, and why a 416 carries the session coordinates a client resumes from."
weight = 5
+++

Three points on the OCI serving path share a theme: peryx tells a client the truth early, and never leaves state behind
that a client cannot see or recover. This page explains the reasoning behind the referrers digest check, the upload
cancel, and the `416` resume headers, and the failures each one prevents. The wire details are in
[upload sessions and referrers digests](@/ecosystems/oci/reference/upload-sessions.md).

## Why a malformed referrers digest is a 400, not an empty 200

`GET /v2/<name>/referrers/<digest>` answers with the manifests that named `<digest>` as their subject. peryx used to let
a bad digest such as `sha256:bad` fall through to a lookup that found nothing and answered `200` with an empty index.
The empty list reads as "nothing refers to this subject", when the real answer is "that is not a subject". A client
trusts the `200`, concludes the artifact carries no signatures or SBOMs, and moves on, having sent a digest the registry
never parsed.

The [distribution spec](https://github.com/opencontainers/distribution-spec) closes that gap: the referrers API must
answer `400 DIGEST_INVALID` when the subject digest has invalid syntax. peryx now validates the digest against the
image-spec grammar before the lookup, so a malformed digest is a hard error the client can see and fix, and only a
well-formed subject reaches the lookup. A well-formed but unknown subject still answers `200` with an empty list,
because there the empty answer is true. The validation stays narrow: it enforces the fixed hex length of the registered
`sha256` and `sha512` algorithms, where an off-length encoding cannot be right, and leaves an unregistered algorithm to
the general grammar rather than guess at an encoding peryx does not define. It refuses the digests that are broken on
their face instead of answering them with a plausible falsehood.

## Why cancelling an upload unlinks the staged file

A chunked upload stages bytes in a temp file that the session owns, and the session lives in the serving process. If a
push stops partway, without a crash, without a `PUT`, that file has no natural end. peryx reaps an idle session after an
hour, so nothing leaks forever, but an hour is a long time to hold disk for a client that already knows it is done: a CI
job that failed its build, a script that opened a session it will not use, a client that changed its mind. Multiply that
by a busy registry and the staged files a client abandoned can outweigh the ones it will finish.

End-14 of the spec gives the client the verb to say so. A `DELETE` on the session URL drops the session and unlinks its
staged file at once, turning "wait out the timeout" into "reclaim now". The registry does not have to guess whether an
open session is alive or forgotten; the client that owns it says. The idle timeout stays as the backstop for the client
that vanishes without a word, and cancel is the fast path for the one that is still present and knows it is finished.
Answering `404` for an unknown session keeps the operation honest in the same way the referrers check does: a `DELETE`
of a session that never existed, or was already committed or cancelled, is not silently accepted as if it did something.

## Why a 416 carries the session coordinates

A chunked upload is a contract about order: each chunk must begin exactly where the last one ended. When a chunk breaks
that contract, out of order, or with a `Content-Range` peryx cannot read, the honest response is to refuse it and keep
the bytes already staged, so the client can resend the one chunk rather than re-upload the whole blob. peryx answers
`416 Range Not Satisfiable` and holds its ground.

But a refusal a client cannot act on is only half an answer. A bare `416` with the current offset tells the client how
far it got, yet a client that has lost its place also needs to know *where* to resume: the session URL and its id. peryx
now returns `Location`, `Docker-Upload-UUID`, and `Range` on the `416`, the same coordinates every other upload response
carries. The `Range` says how many bytes landed, and the `Location` and `Docker-Upload-UUID` say which session to
continue against. A client that overshot can reconstruct the exact next request and pick up where it left off, instead
of tearing down a mostly finished upload and starting over. The session is not lost by the error; it is described by it.

## The thread through all three

Each change replaces a quiet, lossy behavior with a truthful, recoverable one. The referrers check refuses to answer a
broken question with a fake answer. Cancel lets a client reclaim state the moment it knows it is dead, instead of
leaving the server to time it out. The `416` hands back the coordinates to continue instead of only reporting failure. A
strict client, and a conformance suite, reads each of these as the spec mandates; a lenient client sees a registry that
fails in a way it can understand and act on.

## See also

- [Upload sessions and referrers digests](@/ecosystems/oci/reference/upload-sessions.md): the statuses, headers, and
  digest grammar in full.
- [Push a blob chunk by chunk](@/ecosystems/oci/tutorials/chunked-upload.md): the upload state machine, driven by hand.
- [Cancel and resume an OCI push](@/ecosystems/oci/guides/cancel-and-resume-push.md): the two recovery moves as recipes.
- [Standards](@/ecosystems/oci/reference/standards.md): the OCI specifications peryx implements.
