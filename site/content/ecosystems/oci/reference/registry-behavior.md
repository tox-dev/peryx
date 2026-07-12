+++
title = "Registry behavior"
description = "The exact rules for content digest algorithms, upload-session cancel and the 416 resume response, and referrers subject-digest validation, with every status, header, and digest table."
weight = 5
aliases = [
    "/ecosystems/oci/reference/content-digests/",
    "/ecosystems/oci/reference/upload-sessions/",
]
+++

This page states the wire behavior of the OCI conformance points peryx implements on the `/v2/` surface: which digest
algorithms it accepts where, the statuses and headers for cancelling and resuming an upload, and how the referrers API
validates its subject digest. For the full route list see [HTTP endpoints](@/ecosystems/oci/reference/endpoints.md); for
the specifications, see [standards](@/ecosystems/oci/reference/standards.md); for the reasoning, see
[why peryx serves the registry the way it does](@/ecosystems/oci/registry-behavior.md).

## Content digest algorithms

Every object peryx stores is addressed by the sha256 of its exact bytes. A request, or an upstream, can still name a
manifest with a digest in another algorithm the
[image-spec digest grammar](https://github.com/opencontainers/image-spec/blob/main/descriptor.md#digests) permits. This
section states what peryx accepts, how it validates a digest, and where the answer differs between manifests and blobs.
For the reason behind the manifest behavior, see
[why peryx accepts a non-sha256 content digest](@/ecosystems/oci/registry-behavior.md#content-digests).

### The grammar peryx parses

A `<reference>` that contains a `:` is a digest, `algorithm:encoded`; otherwise it is a tag. peryx accepts a digest
reference when both halves are well formed:

- **algorithm**: a non-empty run of lowercase letters, digits, and the separators `+ . _ -`. `sha256`, `sha512`, and a
  custom token like `multihash+base58` all pass.
- **encoded**: a non-empty run of lowercase letters, digits, and `= _ -`. An uppercase letter in the encoded half is
  rejected, because a digest is a cache and storage key and `sha256:AB…` would key a second copy of the same content.

Routing checks the shape only, not the length, and hands the digest on verbatim. A reference that fails the shape does
not route: a manifest or blob request with a malformed digest is not a recognized route and answers `404`.

### Manifest reads

peryx addresses every manifest it stores by the sha256 of its bytes, so a stored manifest's `Docker-Content-Digest` is
always a `sha256:` value. The integrity check that a pull-through runs, whether the bytes hash to what was advertised,
only means something for an algorithm peryx can recompute, which is sha256. It is scoped to a `sha256:` advertisement; a
digest in another algorithm is content-addressed under peryx's own sha256 instead of compared.

| Read                                                                        | peryx does                                                                 |
| --------------------------------------------------------------------------- | -------------------------------------------------------------------------- |
| by tag, upstream advertises a matching `sha256:` digest                     | stores and serves it; `Docker-Content-Digest` is that sha256               |
| by tag, upstream advertises a `sha256:` digest that does not hash the bytes | `502` gateway error; nothing cached                                        |
| by tag, upstream advertises a non-sha256 digest (e.g. `sha512:`)            | stores under the canonical sha256; serves with that sha256, not the sha512 |
| by tag, upstream advertises no digest                                       | stores and serves under the canonical sha256                               |
| by `sha256:` digest that hashes the bytes                                   | serves it                                                                  |
| by `sha256:` digest that does not hash the bytes                            | `400 MANIFEST_INVALID`                                                     |
| by non-sha256 digest (e.g. `sha512:`)                                       | serves the bytes under the requested digest, which it echoes back          |

A pull by a non-sha256 digest can never equal the sha256 canonical, so peryx cannot verify the request against the bytes
the way it does for `sha256:`. The upstream content-addressed the manifest under that digest; peryx serves those bytes
under the digest the client asked for, and stores them under its own sha256 for the cache.

### Blobs are sha256 only

A blob digest on a pull, a mount, or the `PUT` that commits an upload must be `sha256:`. Any other algorithm answers
`400 DIGEST_INVALID` with `only sha256 blob digests are supported`. peryx streams a blob into a content-addressed store
and verifies it against its sha256 on commit, so it has no store keyed by another algorithm to serve one from.

### What content-addressing does not do

- It never stores or keys an object under a non-sha256 digest. Everything on disk is addressed by sha256; a non-sha256
  digest is a value peryx echoes on a read, not a second content address.
- It does not verify a non-sha256 upstream advertisement. It cannot recompute a sha512, so it trusts that header field
  and relies on its own sha256 over the exact bytes for integrity.
- The offline mirror still pins a by-digest reference to sha256. A [mirror](@/ecosystems/oci/guides/air-gapped.md) entry
  written as `repo@sha512:…` fails, because the mirror compares the reference against the sha256 it computes. The
  relaxation is on the online pull-through path, not the mirror pin.

## Upload sessions

An upload session belongs to a hosted index and lives in the serving process. Writing to one needs the target index's
`upload_token` as the Basic-auth password, like every other write. Sessions are held in memory, so a restart drops every
open session and a subsequent request against it is `404`.

### Cancel an upload session

`DELETE /v2/<name>/blobs/uploads/<session>` cancels an open upload (distribution-spec end-14).

| Condition                                                  | Status                    |
| ---------------------------------------------------------- | ------------------------- |
| `<session>` is an open session this index opened           | `204 No Content`          |
| `<session>` is unknown, already committed, or already gone | `404 BLOB_UPLOAD_UNKNOWN` |
| Missing or wrong `upload_token`                            | `401 UNAUTHORIZED`        |
| The resolved index is read-only or has uploads disabled    | `403 DENIED`              |

A `204` drops the session and unlinks its staged temp file. An open session that is neither finished nor cancelled is
reaped on its own after one hour of inactivity.

### The 416 resume response

`PATCH /v2/<name>/blobs/uploads/<session>` appends a chunk only when its `Content-Range` begins exactly where the last
chunk ended. A chunk that starts anywhere else, or whose `Content-Range` cannot be parsed, is
`416 Range Not Satisfiable`, and the session keeps the bytes it already holds so the client can resend rather than
restart. The `416` carries the session coordinates:

| Header               | Value                                | Meaning                                           |
| -------------------- | ------------------------------------ | ------------------------------------------------- |
| `Location`           | `/v2/<name>/blobs/uploads/<session>` | the URL to resume against                         |
| `Docker-Upload-UUID` | `<session>`                          | the session id                                    |
| `Range`              | `0-<end>`                            | the bytes already received; resume at `<end> + 1` |

These are the same coordinates the opening `202`, a chunk `202`, and the progress `GET` (`204`) return, so a client that
overshoots has everything it needs to continue. A `PUT` whose trailing body starts at the wrong offset returns the same
`416`.

## Referrers subject-digest validation

`GET /v2/<name>/referrers/<digest>` validates `<digest>` against the image-spec digest grammar before it looks anything
up. A malformed digest is `400 DIGEST_INVALID` (`referrers digest is malformed`); a well-formed one that names no
subject is `200` with an empty `manifests` list, not an error. This is the one place a digest route answers
`400 DIGEST_INVALID` for a malformed value where a manifest or blob route simply `404`s.

The grammar is `algorithm:encoded`. For the two registered algorithms peryx enforces the fixed lowercase-hex length; an
unregistered algorithm is held only to the general grammar, since peryx cannot know its encoding.

| Algorithm | Encoded length | Character set         |
| --------- | -------------- | --------------------- |
| `sha256`  | 64             | lowercase hex         |
| `sha512`  | 128            | lowercase hex         |
| any other | non-empty      | `[a-z0-9=_-]` grammar |

| `<digest>`                            | Result               | Why                                                     |
| ------------------------------------- | -------------------- | ------------------------------------------------------- |
| `sha256:` + 64 lowercase-hex chars    | `200`                | registered, correct length                              |
| `sha512:` + 128 lowercase-hex chars   | `200`                | registered, correct length                              |
| `sha256:bad`                          | `400 DIGEST_INVALID` | registered but not 64 hex chars                         |
| `sha256:` + 64 non-hex chars          | `400 DIGEST_INVALID` | registered but the encoding is not hex                  |
| `sha256:` + uppercase hex             | `400 DIGEST_INVALID` | a digest keys the store, which is lowercase-only        |
| `sha512:` + 64 hex chars              | `400 DIGEST_INVALID` | registered but the wrong length for `sha512`            |
| `multihash:<non-empty encoding>`      | `200`                | unregistered algorithm, accepted by the general grammar |
| `sha256:` (empty encoding), `nocolon` | `400 DIGEST_INVALID` | not `algorithm:encoded`                                 |

A `200` with an unknown-but-valid subject returns the image-index shape (`application/vnd.oci.image.index.v1+json`,
`schemaVersion: 2`) with `manifests: []`. Before this validation a malformed subject fell through to an empty lookup and
answered `200` with an empty index, hiding the client's mistake.

### What the referrers grammar does not do

The lenient referrers-subject grammar covers `sha512` and unregistered algorithms because a subject is only a lookup
key. Stored content is stricter: peryx addresses and serves **`sha256` blobs and manifests only**. A blob or manifest
`GET`/`PUT`/`DELETE` whose `<digest>` is not `sha256:<64 hex>` is `400 DIGEST_INVALID`, and a `PUT` whose bytes do not
hash to the claimed `sha256` digest is rejected on commit. peryx does not persist a `sha512` object; the algorithm is
accepted on the referrers path as a syntactically valid subject, nothing more.
</content>
