+++
title = "Front a registry that uses non-sha256 digests"
description = "Proxy an upstream that advertises manifest digests in sha512 or another registered algorithm, know which digest peryx reports to your clients, and pin images by it."
weight = 7
+++

Most registries content-address with sha256, but the OCI spec allows sha512 and other registered algorithms, and some
registries advertise their `Docker-Content-Digest` in one of them. peryx proxies such an upstream with no special
configuration; this guide covers what to expect and the two things to get right, which digest to pin by and where the
support stops.

## Point a cached index at it

Nothing about the digest algorithm is configurable, so a cached index is the usual one:

```toml
# peryx.toml
[[index]]
name = "reg"
route = "reg"
ecosystem = "oci"
cached = "https://registry.example.com"
```

Pull as normal. peryx fetches the manifest, hashes the exact bytes under its own sha256, and serves them:

```shell
crane manifest --insecure 127.0.0.1:4433/reg/team/app:1.0
```

## Pin by the digest peryx reports, not the upstream's

peryx addresses every manifest it stores by sha256, so the digest it hands your clients for a tag pull is a `sha256:`
value, even when the upstream advertised sha512. Read it from the response header:

```shell
curl -sI http://127.0.0.1:4433/v2/reg/team/app/manifests/1.0 | grep -i docker-content-digest
```

Pin deployments to that sha256. It is the digest peryx serves the image under and the one a client verifies the bytes
against. If you carry an upstream sha512 digest from elsewhere, a pull by it still works, and peryx serves the bytes
under the digest you request and echoes it back:

```shell
crane manifest --insecure 127.0.0.1:4433/reg/team/app@sha512:<hex>
```

## What still requires sha256

The relaxation is scoped to reading a manifest through a proxy. Three things stay sha256 only:

- **Blobs.** A blob pull, mount, or upload commit must use `sha256:`; any other algorithm answers `400 DIGEST_INVALID`
  with `only sha256 blob digests are supported`. A client that pushes a blob under a non-sha256 digest is rejected.
- **A wrong sha256 advertisement.** If the upstream advertises a `sha256:` digest that does not hash the bytes it sent,
  that is a corrupting hop, and peryx returns `502` and caches nothing, unchanged.
- **Offline mirror pins.** A [mirror](@/ecosystems/oci/guides/air-gapped.md) entry pinned by digest must be `sha256:`.
  `repo@sha512:…` fails the mirror's own sha256 comparison; mirror by tag instead, which stores under the canonical
  sha256.

## Verify

Confirm a tag pull succeeds and reports a sha256 digest:

```shell
curl -si http://127.0.0.1:4433/v2/reg/team/app/manifests/1.0 | head -3
```

A `200` with a `docker-content-digest: sha256:…` line is the proxy working. A `502` means the upstream advertised a
`sha256:` digest that did not match its bytes, a corrupting proxy between you and the upstream rather than an algorithm
peryx declined. The exact rules are in [content digest algorithms](@/ecosystems/oci/reference/content-digests.md), and
the reasoning in [why peryx accepts a non-sha256 content digest](@/ecosystems/oci/content-digest-algorithms.md).
