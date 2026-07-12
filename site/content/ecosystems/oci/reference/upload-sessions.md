+++
title = "Upload sessions and referrers digests"
description = "The exact statuses and headers peryx returns for upload-session DELETE and the 416 resume response, and how it validates a referrers subject digest."
weight = 5
+++

This page states the wire behavior of three conformance points on the OCI `/v2/` surface: cancelling an upload session,
the `416` a chunked upload can return, and how the referrers API validates its subject digest. For the full route list
see [HTTP endpoints](@/ecosystems/oci/reference/endpoints.md); for the specifications, see
[standards](@/ecosystems/oci/reference/standards.md).

## Cancel an upload session

`DELETE /v2/<name>/blobs/uploads/<session>` cancels an open upload (distribution-spec end-14). It needs the target
hosted index's `upload_token` as the Basic-auth password, like every other write.

| Condition                                                  | Status                    |
| ---------------------------------------------------------- | ------------------------- |
| `<session>` is an open session this index opened           | `204 No Content`          |
| `<session>` is unknown, already committed, or already gone | `404 BLOB_UPLOAD_UNKNOWN` |
| Missing or wrong `upload_token`                            | `401 UNAUTHORIZED`        |
| The resolved index is read-only or has uploads disabled    | `403 DENIED`              |

A `204` drops the session and unlinks its staged temp file. Sessions are held in memory on the serving process, so a
restart drops every open session and a `DELETE` afterward is `404`. An open session that is neither finished nor
cancelled is reaped on its own after one hour of inactivity.

## The 416 resume response

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
subject is `200` with an empty `manifests` list, not an error.

The grammar is `algorithm:encoded`. For the two registered algorithms peryx enforces the fixed lowercase-hex length; an
unregistered algorithm is held only to the general grammar, since peryx cannot know its encoding.

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

## What peryx does not do

The lenient referrers-subject grammar covers `sha512` and unregistered algorithms because a subject is only a lookup
key. Stored content is stricter: peryx addresses and serves **`sha256` blobs and manifests only**. A blob or manifest
`GET`/`PUT`/`DELETE` whose `<digest>` is not `sha256:<64 hex>` is `400 DIGEST_INVALID`, and a `PUT` whose bytes do not
hash to the claimed `sha256` digest is rejected on commit. peryx does not persist a `sha512` object; the algorithm is
accepted on the referrers path as a syntactically valid subject, nothing more.
