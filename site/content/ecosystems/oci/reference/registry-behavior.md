+++
title = "Registry behavior"
description = "Digest, upload-session, and referrers rules with their status codes and headers."
weight = 5
aliases = [ "/ecosystems/oci/reference/content-digests/", "/ecosystems/oci/reference/upload-sessions/"]
+++

peryx accepts OCI digest algorithms according to route, returns explicit upload cancel/resume state, and validates each
referrers subject digest. See [HTTP endpoints](@/ecosystems/oci/reference/endpoints.md),
[standards](@/ecosystems/oci/reference/standards.md), and
[registry behavior decisions](@/ecosystems/oci/registry-behavior.md).

## Content digest algorithms

Peryx addresses stored objects by the sha256 of its exact bytes. A request, or an upstream, can still name a manifest
with a digest in another algorithm the
[image-spec digest grammar](https://github.com/opencontainers/image-spec/blob/main/descriptor.md) permits. peryx applies
different validation rules to manifests and blobs. For the reason behind the manifest behavior, see
[why peryx accepts a non-sha256 content digest](@/ecosystems/oci/registry-behavior.md#digest-handling).

### Parsed digest grammar

A `<reference>` that contains a `:` is a digest, `algorithm:encoded`; otherwise it is a tag. peryx accepts a digest
reference when both halves are well formed:

- **algorithm**: a non-empty run of lowercase letters, digits, and the separators `+ . _ -`. `sha256`, `sha512`, and a
  custom token like `multihash+base58` all pass.
- **encoded**: a non-empty run of lowercase letters, digits, and `= _ -`. An uppercase letter in the encoded half is
  rejected, because a digest is a cache and storage key and `sha256:AB…` would key a second copy of the same content.

Routing checks the shape only, not the length, and hands the digest on verbatim. A reference that fails the shape does
not route: a manifest or blob request with a malformed digest is not a recognized route and answers `404`.

### Manifest reads

peryx addresses stored manifests by the sha256 of its bytes, so a stored manifest's `Docker-Content-Digest` is always a
`sha256:` value. The pull-through integrity check, whether the bytes hash to what was advertised, only means something
for an algorithm peryx can recompute, which is sha256. It is scoped to a `sha256:` advertisement; a digest in another
algorithm is content-addressed under peryx's own sha256 instead of compared.

| Read                                                           | Result                                                         |
| -------------------------------------------------------------- | -------------------------------------------------------------- |
| Tag; upstream advertises a matching `sha256:` digest           | Store and serve it with that sha256 in `Docker-Content-Digest` |
| Tag; upstream advertises a mismatched `sha256:` digest         | Return `502`; cache nothing                                    |
| Tag; upstream advertises a non-sha256 digest such as `sha512:` | Store and serve under the canonical sha256                     |
| Tag; upstream advertises no digest                             | Store and serve under the canonical sha256                     |
| Matching `sha256:` digest                                      | Serve the manifest                                             |
| Mismatched `sha256:` digest                                    | Return `400 MANIFEST_INVALID`                                  |
| Non-sha256 digest such as `sha512:`                            | Serve the bytes and echo the requested digest                  |

A pull by a non-sha256 digest cannot equal the sha256 canonical, so peryx cannot verify the request against the bytes
the way it does for `sha256:`. The upstream content-addressed the manifest under that digest; peryx serves those bytes
under the digest the client asked for, and stores them under its own sha256 for the cache.

### Blobs are sha256 only

A blob digest on a pull, a mount, or the `PUT` that commits an upload must be `sha256:`. Any other algorithm answers
`400 DIGEST_INVALID` with `only sha256 blob digests are supported`. peryx streams a blob into a content-addressed store
and verifies it against its sha256 on commit, so it has no store keyed by another algorithm to serve one from.

### Repository membership

peryx stores one copy of a blob and grants access through separate `(index, repository, digest)` links. Reads and
deletes use the repository link. A mount checks the source link and pull permission before peryx copies it to the
target.

### Content-addressing scope

- peryx does not store or key an object under a non-sha256 digest. Everything on disk is addressed by sha256; a
  non-sha256 digest is a value peryx echoes on a read, not a second content address.
- It does not verify a non-sha256 upstream advertisement. It cannot recompute a sha512, so it trusts that header field
  and relies on its own sha256 over the exact bytes for integrity.
- The offline mirror still pins a by-digest reference to sha256. A [mirror](@/ecosystems/oci/guides/air-gapped.md) entry
  written as `repo@sha512:…` fails, because the mirror compares the reference against the sha256 it computes. The
  relaxation is on the online pull-through path, not the mirror pin.

## Upload sessions

The opening request records its complete `<name>` in the upload session. peryx encodes a 128-bit random id as 32
lowercase hexadecimal characters. For a continuation request, peryx checks write access for the requested repository and
then compares both stored scope values. A credential with write access may continue when the repository matches. Since
peryx holds sessions in memory, a restart drops open sessions and later requests receive `404`.

### Cancel an upload session

`DELETE /v2/<name>/blobs/uploads/<session>` cancels an open upload (distribution-spec end-14).

| Condition                                                     | Status                    |
| ------------------------------------------------------------- | ------------------------- |
| Request matches the recorded index and complete `<name>`      | `204 No Content`          |
| Session is unknown, expired, or belongs to another repository | `404 BLOB_UPLOAD_UNKNOWN` |
| Credential lacks write access to the requested repository     | `401 UNAUTHORIZED`        |
| Index configuration disallows uploads                         | `403 DENIED`              |

A `204` drops the session and unlinks its staged temp file. peryx expires an unfinished session after one hour without a
status `GET` or `PATCH` attempt. The once-per-minute sweep removes expired sessions within the next minute; starting
another session runs the same expiry pass. When a client changes the name, peryx keeps the original session and staged
bytes unchanged.

The hosted index's `max_file_size_bytes` applies while a monolithic `POST`, chunk `PATCH`, or final `PUT` streams. peryx
checks the cumulative byte count before writing each chunk. An over-limit request returns `403 DENIED`; for a chunked
upload, it also removes the session and its staged file, so later requests for that id return `404 BLOB_UPLOAD_UNKNOWN`.

### 416 resume response

`PATCH /v2/<name>/blobs/uploads/<session>` appends a chunk only when its `Content-Range` begins exactly where the last
chunk ended. A chunk that starts anywhere else, or whose `Content-Range` cannot be parsed, is
`416 Range Not Satisfiable`, and the session keeps the bytes it already holds so the client can resend rather than
restart. The `416` carries the session coordinates:

| Header               | Value                                | Meaning                               |
| -------------------- | ------------------------------------ | ------------------------------------- |
| `Location`           | `/v2/<name>/blobs/uploads/<session>` | Resume URL                            |
| `Docker-Upload-UUID` | `<session>`                          | Session ID                            |
| `Range`              | `0-<end>`                            | Received bytes; resume at `<end> + 1` |

These are the same coordinates the opening `202`, a chunk `202`, and the progress `GET` (`204`) return, so a client that
overshoots has everything it needs to continue. A `PUT` whose trailing body starts at the wrong offset returns the same
`416`.

## Referrers subject-digest validation

`GET /v2/<name>/referrers/<digest>` validates `<digest>` against the image-spec digest grammar before it looks anything
up. A malformed digest is `400 DIGEST_INVALID` (`referrers digest is malformed`); a well-formed one that names no
subject is `200` with an empty `manifests` list, not an error. This is the one place a digest route answers
`400 DIGEST_INVALID` for a malformed value where a manifest or blob route returns `404`.

The grammar is `algorithm:encoded`. For the two registered algorithms peryx enforces the fixed lowercase-hex length; an
unregistered algorithm is held only to the general grammar, since peryx cannot know its encoding.

| Algorithm | Encoded length | Character set         |
| --------- | -------------- | --------------------- |
| `sha256`  | 64             | Lowercase hex         |
| `sha512`  | 128            | Lowercase hex         |
| Any other | Non-empty      | `[a-z0-9=_-]` grammar |

| `<digest>`                                  | Result               | Reason                                          |
| ------------------------------------------- | -------------------- | ----------------------------------------------- |
| `sha256:` plus 64 lowercase-hex characters  | `200`                | Registered algorithm and correct length         |
| `sha512:` plus 128 lowercase-hex characters | `200`                | Registered algorithm and correct length         |
| `sha256:bad`                                | `400 DIGEST_INVALID` | Wrong length for sha256                         |
| `sha256:` plus 64 non-hex characters        | `400 DIGEST_INVALID` | sha256 encoding must be hexadecimal             |
| `sha256:` plus uppercase hex                | `400 DIGEST_INVALID` | Store keys use lowercase digests                |
| `sha512:` plus 64 hex characters            | `400 DIGEST_INVALID` | Wrong length for sha512                         |
| `multihash:<non-empty encoding>`            | `200`                | General grammar permits unregistered algorithms |
| `sha256:` or `nocolon`                      | `400 DIGEST_INVALID` | Value does not match `algorithm:encoded`        |

A `200` with an unknown-but-valid subject returns the image-index shape (`application/vnd.oci.image.index.v1+json`,
`schemaVersion: 2`) with `manifests: []`. Before this validation a malformed subject fell through to an empty lookup and
answered `200` with an empty index, hiding the client's mistake.

### Referrers-grammar scope

The lenient referrers-subject grammar covers `sha512` and unregistered algorithms because a subject is only a lookup
key. Stored content is stricter: peryx addresses and serves **`sha256` blobs and manifests only**. A blob or manifest
`GET`/`PUT`/`DELETE` whose `<digest>` is not `sha256:<64 hex>` is `400 DIGEST_INVALID`, and a `PUT` whose bytes do not
hash to the claimed `sha256` digest is rejected on commit. peryx does not persist a `sha512` object; the algorithm is
accepted on the referrers path as a syntactically valid subject, nothing more.
