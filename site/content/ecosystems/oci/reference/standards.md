+++
title = "Standards"
description = "The OCI specifications peryx implements for container images, and how they fit together."
weight = 1
+++

peryx targets the specifications a modern container registry and its clients rely on. The
[OCI distribution spec](https://github.com/opencontainers/distribution-spec) defines the `/v2/` HTTP API; the
[image spec](https://github.com/opencontainers/image-spec) defines the manifests and blobs that flow over it. peryx
answers the version check with `Docker-Distribution-API-Version: registry/2.0`.

## What a docker pull asks for

Knowing the request sequence makes the table below concrete. For `docker pull alpine:latest` against any
distribution-spec registry:

{% mermaid() %}
sequenceDiagram
participant D as docker / podman
participant R as registry
D->>+R: GET /v2/ (version check)
R-->>-D: 200, Docker-Distribution-API-Version
D->>+R: GET /v2/<name>/manifests/latest (Accept: image manifest)
R-->>-D: manifest JSON: config + layer descriptors, digests
D->>+R: GET /v2/<name>/blobs/sha256:… (config, then each layer)
R-->>-D: the blob, which docker verifies against its digest
{% end %}

Every hop names a spec: the routes are the distribution spec, the manifest and blob shapes are the image spec, each
digest is the content-addressing both rely on. peryx sits on both sides of this conversation, a registry to your
clients and a client to its upstreams, which is why the table below mixes "served" and "parsed".

| Standard                                                                                                 | Role in peryx                                                                                                                       |
| -------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------- |
| [Distribution spec](https://github.com/opencontainers/distribution-spec)                                 | The `/v2/` pull-and-push API: manifests, blobs, chunked uploads, cross-repo mount, tag listing; served to clients and spoken upstream |
| [Image spec: manifest](https://github.com/opencontainers/image-spec/blob/main/manifest.md)               | The manifest JSON listing a config and layer descriptors; stored byte-for-byte and addressed by the sha256 of those exact bytes       |
| [Image spec: image index](https://github.com/opencontainers/image-spec/blob/main/image-index.md)         | Multi-platform indexes and the referrers response, served as `application/vnd.oci.image.index.v1+json`                                |
| [Image spec: descriptor](https://github.com/opencontainers/image-spec/blob/main/descriptor.md)           | `mediaType`, `digest`, `size`, `artifactType`, and `annotations` on every referenced object                                           |
| [Referrers API](https://github.com/opencontainers/distribution-spec/blob/main/spec.md#listing-referrers) | `GET /v2/<name>/referrers/<digest>` returning the manifests that declared `<digest>` as their `subject` (`OCI-Subject` on push)       |
| [Docker manifest v2, schema 2](https://distribution.github.io/distribution/spec/manifest-v2-2/)          | The Docker-media-type manifests and image indexes that Docker Hub and older clients still emit; parsed and re-served                  |
| [Token authentication](https://distribution.github.io/distribution/spec/auth/token/)                     | The `401` + `WWW-Authenticate: Bearer` handshake peryx runs as a *client* against an upstream that demands it                       |

## Digests are the contract

Every manifest and blob is addressed by `sha256:<hex>` over its exact bytes. peryx stores a manifest byte-for-byte, so
the `Docker-Content-Digest` a client verifies always matches what it pushed or pulled, and a blob shared by ten images
is stored once. A digest in any other algorithm is rejected with `400 DIGEST_INVALID` rather than served unverified.

## Graceful degradation

Upstreams differ in what they emit. Docker Hub and GHCR serve Docker schema-2 media types where a private registry may
serve OCI ones; peryx parses both and preserves the stored `Content-Type` on the way back out, so a client sees the
media type the source produced. A pull-through that fails or answers unexpectedly returns `502` with code `UNKNOWN`, so
a gateway fault is never mistaken for a client error the puller would not retry.

Pulls take no authentication; the bearer-token handshake belongs to the pull-through path, where peryx fetches and
caches a token per scope against an upstream that challenges it. Writes require a Basic-auth token, the target hosted
index's `upload_token`, which is why `docker login` against peryx uses the token as the password.

## In practice

- The machinery that serves these: [architecture](@/core/architecture.md)
- The routes they map to: [HTTP endpoints](@/ecosystems/oci/reference/endpoints.md)
