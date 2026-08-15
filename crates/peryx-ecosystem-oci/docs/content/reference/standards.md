+++
title = "Standards"
description = "OCI distribution, image, descriptor, referrer, and authentication specifications."
weight = 1
+++

peryx targets the specifications a modern container registry and its clients rely on. The
[OCI distribution spec](https://github.com/opencontainers/distribution-spec) defines the `/v2/` HTTP API; the
[image spec](https://github.com/opencontainers/image-spec) defines the manifests and blobs that flow over it. peryx
answers the version check with `Docker-Distribution-API-Version: registry/2.0`.

## Pull request sequence

`docker pull alpine:latest` sends this sequence to a distribution-spec registry:

{% mermaid() %} sequenceDiagram participant D as docker / podman participant R as registry D->>+R: GET /v2/ (version
check) R-->>-D: 200, Docker-Distribution-API-Version D->>+R: GET /v2/<name>/manifests/latest (Accept: image manifest)
R-->>-D: manifest JSON: config + layer descriptors, digests D->>+R: GET /v2/<name>/blobs/sha256:... (config, then each
layer) R-->>-D: blob, which docker verifies against its digest {% end %}

The distribution spec defines the routes. The image spec defines manifest and blob shapes. peryx serves these formats to
clients and parses them from upstreams.

| Standard                                                                                        | Role in peryx                                                                    |
| ----------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------- |
| [Distribution spec](https://github.com/opencontainers/distribution-spec)                        | `/v2/` pull and push routes for manifests, blobs, uploads, mounts, and tags      |
| [Image manifest](https://github.com/opencontainers/image-spec/blob/main/manifest.md)            | Manifest JSON stored byte-for-byte under its sha256 digest                       |
| [Image index](https://github.com/opencontainers/image-spec/blob/main/image-index.md)            | Multi-platform indexes and referrers responses                                   |
| [Descriptor](https://github.com/opencontainers/image-spec/blob/main/descriptor.md)              | `mediaType`, `digest`, `size`, `artifactType`, and `annotations`                 |
| [Referrers API](https://github.com/opencontainers/distribution-spec/blob/main/spec.md)          | `GET /v2/<name>/referrers/<digest>` and `OCI-Subject` on push                    |
| [Docker manifest v2, schema 2](https://distribution.github.io/distribution/spec/manifest-v2-2/) | Docker media types emitted by Docker Hub and older clients                       |
| [Token authentication](https://distribution.github.io/distribution/spec/auth/token/)            | Bearer challenges, token minting, grant enforcement, and upstream authentication |

## Digest addressing

Peryx addresses each manifest and blob by `sha256:<hex>` over its exact bytes. It stores a manifest byte-for-byte, so
the `Docker-Content-Digest` matches what the client pushed or pulled, and a blob shared by ten images is stored once. A
blob digest in any other algorithm is rejected with `400 DIGEST_INVALID` rather than served unverified; a manifest an
upstream advertises under another algorithm is re-addressed under peryx's own sha256, covered in
[content digest algorithms](registry-behavior.md#content-digest-algorithms).

## Upstream compatibility

Upstreams differ in what they emit. [Docker Hub](https://hub.docker.com/) and [GHCR](https://docs.github.com/packages)
serve Docker schema-2 media types where a private registry may serve OCI ones. peryx parses both and preserves the
stored `Content-Type`. A pull-through failure or invalid response returns `502` with code `UNKNOWN`, which distinguishes
gateway failures from request errors.

Public OCI indexes permit anonymous pulls. Restricted indexes mint Bearer tokens at `/v2/token` and enforce their
grants. For an upstream challenge, peryx fetches and caches one upstream token per scope. Writes require the secret from
a write-granting `[[index.access_token]]` on the hosted index; `docker login` uses that secret as its password.

## Operational checks

- Plugin and runtime boundaries: [architecture](@/core/architecture.md)
- OCI routes: [HTTP endpoints](endpoints.md)
