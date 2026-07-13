+++
title = "Why peryx serves the registry the way it does"
description = "Why peryx accepts an upstream manifest digest in a non-sha256 algorithm, why it validates a referrers subject up front, and why cancelling an upload reclaims its bytes while a 416 hands back the coordinates to resume."
weight = 5
aliases = [ "/ecosystems/oci/content-digest-algorithms/", "/ecosystems/oci/upload-conformance/"]
+++

peryx content-addresses everything it stores under its own sha256, yet it has to interoperate with registries and
clients that use the wider OCI grammar and drive the upload state machine their own way. Two themes run through how it
does that: it reads what an upstream sends rather than only what it would have sent itself, and it tells a client the
truth early, never leaving state behind that a client cannot see or recover. This page explains the reasoning behind the
content-digest handling, the referrers check, and the upload cancel and `416` resume.

## Content digests

The [image-spec digest grammar](https://github.com/opencontainers/image-spec/blob/main/descriptor.md#digests) is
`algorithm:encoded`, and it names more than one algorithm. `sha256` is the common one, `sha512` is registered beside it,
and the grammar leaves room for others. A registry is free to advertise its content address in any of them through the
`Docker-Content-Digest` header. peryx has to read what an upstream sends, not only what it would have sent itself.

### The pull that used to fail

peryx stores a manifest byte-for-byte and addresses it by the sha256 of those exact bytes. When it pulls a manifest
through, it computes that sha256 and, if the upstream advertised a digest, compares the two. The comparison catches a
corrupting proxy or CDN between peryx and the upstream: altered bytes hash to something else, so a mismatch means the
manifest is not what upstream signed for, and peryx refuses to cache it.

That comparison was a plain string equality. An upstream that content-addresses with sha512 advertises `sha512:6910c9…`;
peryx computed `sha256:fc6b27…` over byte-identical content and compared the two strings. They can never be equal, a
different algorithm and a different length, so peryx read every such pull as a corrupted hop, returned `502`, and cached
nothing. A registry that did nothing wrong was unusable through peryx, and no retry could fix it, because the "mismatch"
was structural.

### What the check is actually for

The integrity check earns its place only when peryx can recompute the advertised digest. For `sha256:` it can: it hashes
the bytes itself and a mismatch is real evidence of tampering. For `sha512:` it cannot, because it does not hash the
bytes a second time under sha512, so comparing a sha512 string to a sha256 string proves nothing about the bytes.
Treating that guaranteed inequality as corruption was the bug.

So the check is scoped to a `sha256:` advertisement, the case where it can run. A digest in any other algorithm is not
compared; peryx content-addresses the bytes under its own sha256, which it still computes and verifies, stores them, and
serves them. A wrong `sha256:` advertisement is still rejected exactly as before, because there the comparison is
meaningful.

### Why this keeps the guarantee

peryx's own integrity promise does not change. It still hashes every manifest it stores, serves it under that sha256,
and reports that sha256 in `Docker-Content-Digest`, so a client that pulls the manifest back verifies the digest peryx
computed over the bytes it holds. The only thing dropped is a comparison that could not run in the first place. A pull
by a non-sha256 digest is served under the digest the client asked for, the upstream's content address, while the cache
key stays peryx's sha256.

### The failure it prevents, and the scope

Without this, an entire class of upstream, any registry or client that content-addresses with sha512 or another
registered algorithm, returns `502` on every tag, and interop with a spec-conformant registry breaks on a detail the
spec explicitly allows. Accepting the broader grammar is what lets peryx sit in front of one.

The relaxation is narrow. It applies to the online manifest pull-through path. Blobs are still sha256 only, an offline
mirror still pins a by-digest reference to sha256, and a malformed digest is still rejected. The exact rules are in
[the registry-behavior reference](@/ecosystems/oci/reference/registry-behavior.md#content-digest-algorithms); the
surrounding read path is in [how peryx scopes and serves manifest reads](@/ecosystems/oci/manifest-serving.md).

## Upload sessions and the 416 resume

A chunked upload stages bytes in a temp file that the session owns, and the session lives in the serving process. peryx
never leaves that state stranded: a client that knows it is done can reclaim its bytes at once, and a client that lost
its place is handed back the coordinates to continue.

peryx uses the session id only to locate temporary state. A 128-bit operating-system random value makes active uploads
infeasible to enumerate. The opening repository name stays with the state. For each follow-up, peryx checks write access
and compares the request repository. When a client moves an id under another repository, peryx reports it as unknown and
keeps the original session. Another credential may resume the upload when it can write the same repository because peryx
authenticates each request.

### Why cancelling an upload unlinks the staged file

If a push stops partway, without a crash, without a `PUT`, that staged file has no natural end. peryx reaps an idle
session after an hour, so nothing leaks forever, but an hour is a long time to hold disk for a client that already knows
it is done: a CI job that failed its build, a script that opened a session it will not use, a client that changed its
mind. Multiply that by a busy registry and the staged files a client abandoned can outweigh the ones it will finish.

End-14 of the [distribution spec](https://github.com/opencontainers/distribution-spec) gives the client the verb to say
so. A `DELETE` on the session URL drops the session and unlinks its staged file at once, turning "wait out the timeout"
into "reclaim now". The registry does not have to guess whether an open session is alive or forgotten; the client that
owns it says. The idle timeout stays as the backstop for the client that vanishes without a word, and cancel is the fast
path for the one that is still present and knows it is finished. Answering `404` for an unknown session keeps the
operation honest: a `DELETE` of a session that never existed, or was already committed or cancelled, is not silently
accepted as if it did something.

peryx handles upload expiry from one process-wide minute tick, avoiding a timer task for each upload. On each tick, the
OCI driver takes the session-map lock once and removes entries whose last status check or `PATCH` attempt occurred at
least one hour ago. The process keeps an abandoned session and its open temp file for less than one minute past the idle
deadline.

### Why a 416 carries the session coordinates

A chunked upload is a contract about order: each chunk must begin exactly where the last one ended. When a chunk breaks
that contract, out of order, or with a `Content-Range` peryx cannot read, the honest response is to refuse it and keep
the bytes already staged, so the client can resend the one chunk rather than re-upload the whole blob. peryx answers
`416 Range Not Satisfiable` and holds its ground.

But a refusal a client cannot act on is only half an answer. A bare `416` with the current offset tells the client how
far it got, yet a client that has lost its place also needs to know *where* to resume: the session URL and its id. peryx
returns `Location`, `Docker-Upload-UUID`, and `Range` on the `416`, the same coordinates every other upload response
carries. The `Range` says how many bytes landed, and the `Location` and `Docker-Upload-UUID` say which session to
continue against. A client that overshot can reconstruct the exact next request and pick up where it left off, instead
of tearing down a mostly finished upload and starting over. The session is not lost by the error; it is described by it.

## Referrers validation

`GET /v2/<name>/referrers/<digest>` answers with the manifests that named `<digest>` as their subject. peryx used to let
a bad digest such as `sha256:bad` fall through to a lookup that found nothing and answered `200` with an empty index.
The empty list reads as "nothing refers to this subject", when the real answer is "that is not a subject". A client
trusts the `200`, concludes the artifact carries no signatures or SBOMs, and moves on, having sent a digest the registry
never parsed.

The distribution spec closes that gap: the referrers API must answer `400 DIGEST_INVALID` when the subject digest has
invalid syntax. peryx validates the digest against the image-spec grammar before the lookup, so a malformed digest is a
hard error the client can see and fix, and only a well-formed subject reaches the lookup. A well-formed but unknown
subject still answers `200` with an empty list, because there the empty answer is true. The validation stays narrow: it
enforces the fixed hex length of the registered `sha256` and `sha512` algorithms, where an off-length encoding cannot be
right, and leaves an unregistered algorithm to the general grammar rather than guess at an encoding peryx does not
define. It refuses the digests that are broken on their face instead of answering them with a plausible falsehood.

## The thread through all three

Each change replaces a quiet, lossy behavior with a truthful, recoverable one. Accepting a non-sha256 advertisement
drops a comparison that could never run rather than reading it as corruption. Cancel lets a client reclaim state the
moment it knows it is dead, instead of leaving the server to time it out. The `416` hands back the coordinates to
continue instead of only reporting failure. The referrers check refuses to answer a broken question with a fake answer.
A strict client, and a conformance suite, reads each of these as the spec mandates; a lenient client sees a registry
that fails in a way it can understand and act on.

## See also

- The statuses, headers, and digest grammar in full:
  [registry-behavior reference](@/ecosystems/oci/reference/registry-behavior.md)
- Proxy a non-sha256 registry, and cancel or resume a push:
  [work with registry behavior](@/ecosystems/oci/guides/registry-behavior.md)
- Drive the upload state machine and a sha512 pull by hand:
  [push a blob chunk by chunk](@/ecosystems/oci/tutorials/chunked-upload.md)
- The OCI specifications peryx implements: [standards](@/ecosystems/oci/reference/standards.md) </content>
