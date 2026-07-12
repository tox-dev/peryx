+++
title = "Upload digest fields"
description = "The digest fields peryx accepts on the legacy upload API, which one it verifies, and the errors a wrong or malformed digest returns."
weight = 4
+++

The legacy upload API lets a client declare a content digest of the file it sends. peryx accepts three digest fields and
verifies whichever the client declared against the bytes it staged. A correct digest passes; a wrong one is rejected.
This page states which fields peryx reads, which it verifies, and how a mismatch is reported.

## Accepted fields

An upload's multipart form may carry any of these fields alongside the `content` part:

| Field               | Algorithm   | Hex length |
| ------------------- | ----------- | ---------- |
| `sha256_digest`     | SHA-256     | 64         |
| `blake2_256_digest` | BLAKE2b-256 | 64         |
| `md5_digest`        | MD5         | 32         |

Any one of them suffices, and none is required. peryx always computes the SHA-256 it content-addresses the file by,
independent of what the client declares, so an upload that declares no digest at all is still stored. twine and
`uv publish` normally send all three; older tooling and minimal CI scripts sometimes send `md5_digest` alone.

## What peryx verifies

peryx hashes the staged bytes with SHA-256 and BLAKE2b-256 as it reads the upload stream, so verifying a declared
`sha256_digest` or `blake2_256_digest` costs nothing beyond a comparison. It verifies each field the client declared:

- **`sha256_digest`** against the content SHA-256 it computed.
- **`blake2_256_digest`** against the content BLAKE2b-256 it computed.
- **`md5_digest`** only when it is the sole declared digest, meaning neither `sha256_digest` nor `blake2_256_digest` is
  present. peryx does not compute MD5 while staging, so this is the one case that reads the staged content a second
  time. When a stronger digest is declared, that verification already covers the bytes, and peryx leaves the declared
  MD5 unverified rather than re-reading the file.

The check is the same regardless of field: the declared value must be lowercase hex of the field's length and must equal
the digest peryx computed.

## Rejections

A declared digest that does not match the content is a `400`:

| Condition                                      | Status | Message                                                                 |
| ---------------------------------------------- | ------ | ----------------------------------------------------------------------- |
| `md5_digest` disagrees with the content        | `400`  | `md5_digest mismatch`                                                   |
| `sha256_digest` disagrees with the content     | `400`  | `sha256_digest mismatch`                                                |
| `blake2_256_digest` disagrees with the content | `400`  | `blake2_256_digest mismatch`                                            |
| a digest is not lowercase hex of its length    | `400`  | `<field> value "<value>" is not lowercase hex with the expected length` |

The mismatch message is always `<field> mismatch`, naming the field that disagreed. A wrong `md5_digest` is only reached
when MD5 is the sole declared digest; when a stronger digest is present peryx verifies that one and never inspects the
MD5.

## What peryx does not do

peryx does not advertise MD5 downstream. The simple-index entry for a stored file carries a `sha256` hash and no `md5`,
so clients read and verify the artifact by SHA-256 regardless of which digest the uploader declared. MD5 is a weak hash;
peryx accepts it on upload for parity with the index it fronts, not as a content guarantee it re-serves.

## In practice

- Walk an MD5-only upload end to end: [publish with an MD5-only client](@/ecosystems/pypi/tutorials/md5-upload.md)
- Verify a single-digest upload flow: [upload with one digest](@/ecosystems/pypi/guides/md5-upload.md)
- Why peryx accepts MD5 but never re-serves it: [MD5 on upload](@/ecosystems/pypi/upload-digests.md)
