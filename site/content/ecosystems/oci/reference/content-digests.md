+++
title = "Content digest algorithms"
description = "Which digest algorithms peryx accepts in a manifest reference and an upstream Docker-Content-Digest, the grammar and length it validates, and what it rejects."
weight = 6
+++

Every object peryx stores is addressed by the sha256 of its exact bytes. A request, or an upstream, can still name a
manifest with a digest in another algorithm the
[image-spec digest grammar](https://github.com/opencontainers/image-spec/blob/main/descriptor.md#digests) permits. This
page states what peryx accepts, how it validates a digest, and where the answer differs between manifests and blobs. For
the routes these apply to see [HTTP endpoints](@/ecosystems/oci/reference/endpoints.md); for the reason behind the
manifest behavior see [why peryx accepts a non-sha256 content digest](@/ecosystems/oci/content-digest-algorithms.md).

## The grammar peryx parses

A `<reference>` that contains a `:` is a digest, `algorithm:encoded`; otherwise it is a tag. peryx accepts a digest
reference when both halves are well formed:

- **algorithm**: a non-empty run of lowercase letters, digits, and the separators `+ . _ -`. `sha256`, `sha512`, and a
  custom token like `multihash+base58` all pass.
- **encoded**: a non-empty run of lowercase letters, digits, and `= _ -`. An uppercase letter in the encoded half is
  rejected, because a digest is a cache and storage key and `sha256:AB…` would key a second copy of the same content.

Routing checks the shape only, not the length, and hands the digest on verbatim. A reference that fails the shape does
not route: a manifest or blob request with a malformed digest is not a recognized route and answers `404`.

## Manifest reads

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

## Referrers digests

`GET /v2/<name>/referrers/<digest>` validates its subject digest and answers `400 DIGEST_INVALID` for a malformed one,
where a manifest or blob route simply `404`s. A registered algorithm is held to its fixed lowercase-hex length:

| Algorithm | Encoded length | Character set         |
| --------- | -------------- | --------------------- |
| `sha256`  | 64             | lowercase hex         |
| `sha512`  | 128            | lowercase hex         |
| any other | non-empty      | `[a-z0-9=_-]` grammar |

A `sha256:` or `sha512:` value of the wrong length, or with a non-hex character, is malformed. An unregistered algorithm
keeps only the general grammar peryx cannot second-guess.

## Blobs are sha256 only

A blob digest on a pull, a mount, or the `PUT` that commits an upload must be `sha256:`. Any other algorithm answers
`400 DIGEST_INVALID` with `only sha256 blob digests are supported`. peryx streams a blob into a content-addressed store
and verifies it against its sha256 on commit, so it has no store keyed by another algorithm to serve one from.

## What peryx does not do

- It never stores or keys an object under a non-sha256 digest. Everything on disk is addressed by sha256; a non-sha256
  digest is a value peryx echoes on a read, not a second content address.
- It does not verify a non-sha256 upstream advertisement. It cannot recompute a sha512, so it trusts that header field
  and relies on its own sha256 over the exact bytes for integrity.
- The offline mirror still pins a by-digest reference to sha256. A [mirror](@/ecosystems/oci/guides/air-gapped.md) entry
  written as `repo@sha512:…` fails, because the mirror compares the reference against the sha256 it computes. The
  relaxation is on the online pull-through path, not the mirror pin.
