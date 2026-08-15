+++
title = "OCI"
description = "OCI index roles, the /v2/ distribution protocol, and client configuration."
weight = 2
sort_by = "weight"
template = "section.html"
[extra]
logos = [ "logos/oci.svg"]
logos_dark = [ "logos/oci-white.svg"]
+++

OCI defines the container-image format and the HTTP protocol that clients such as [Docker](https://docs.docker.com/) and
[Podman](https://podman.io/) use to pull and push them. An image is a small tree, not one file. It contains a
**manifest** (a JSON document listing the image's parts), a **config** blob, and one or more **layer** blobs (the
gzip-compressed filesystem). Each **blob** uses the sha256 of its bytes as its address; a mutable **tag** (`latest`,
`1.25`) points at a manifest's digest. peryx serves OCI over the
[distribution spec](https://github.com/opencontainers/distribution-spec) that registries
([Docker Hub](https://hub.docker.com/), [GHCR](https://docs.github.com/packages), [ECR](https://aws.amazon.com/ecr/),
[Artifactory](https://jfrog.com/artifactory/)) implement.

## Terms

OCI routes use Distribution terms: registry, repository, manifest, tag, blob, pull, and push. Peryx configuration uses
the shared index roles **cached**, **hosted**, and **virtual**. A cached OCI index proxies one registry; a hosted index
accepts pushes; a virtual index composes other OCI indexes in order. See [the index model](@/core/indexes.md) for role
and shadowing rules.

## Activation and ownership

The binary includes the OCI implementation. An index with `ecosystem = "oci"` activates it during startup. If the
resolved topology has no OCI index, the plugin installs no routes, services, or background work.

`peryx-ecosystem-oci` owns Distribution routing, manifest and repository metadata, `[index.settings]`, OCI event
payloads, client discovery, search projection, quota accounting, and OCI mutation replay. It implements core capability
traits; crates outside it do not parse manifests or repository records. `[availability]` selects `none`, `dc`, or `ha`
for the process.

## OCI index roles

The three [index roles](@/core/indexes.md) map onto OCI like this:

- **cached**: a read-through cache of an upstream registry. On a miss peryx pulls the manifest or blob from upstream
  (running the bearer-token handshake the registry requires), verifies its digest, stores it, and serves it; later pulls
  come from disk. Point one at Docker Hub, GHCR, or any `/v2/` registry.
- **hosted**: a store for your images. Peryx streams blobs into the content-addressed store and verifies them on commit;
  it keeps manifests byte-for-byte so their digest remains stable. Pushing needs a token (below).
- **virtual**: an ordered stack of members served under one name, where your hosted images shadow same-named upstream
  ones: a pull of a name you have published serves your image, and anything you have not published falls through to the
  upstream. This is the [dependency-confusion defense](@/core/indexes.md#shadowing), applied to containers.

A cached route retries upstream server errors, timeouts, and `429` responses with bounded backoff. A valid `Retry-After`
delay or HTTP date takes precedence, capped at 30 seconds.

## Protocol

Container clients speak the **distribution spec** over a `/v2/` API. peryx implements that API:

- `GET /v2/`: the version check every client pings first; peryx answers `200` with
  `Docker-Distribution-API-Version: registry/2.0`, or a `401` Bearer challenge when an index restricts access.
- **Manifests**: `GET|HEAD|PUT|DELETE /v2/<name>/manifests/<tag-or-digest>`, plus `PUT .../restore`. peryx keeps a
  manifest byte-for-byte and addresses it by the sha256 of those exact bytes, so the `Docker-Content-Digest` a client
  verifies matches.
- **Blobs**: `GET|HEAD|DELETE /v2/<name>/blobs/<digest>`, plus the upload dance
  (`POST`/`PATCH`/`PUT /v2/<name>/blobs/uploads/…`) for push. peryx deduplicates blob bytes across indexes and requires
  a repository link before serving them. For a cross-repo mount, peryx verifies the source link and pull access before
  it adds the target link. Concurrent pulls of one uncached layer share a single upstream fetch.
- **Tags**: `GET /v2/<name>/tags/list`.
- **Token auth**: peryx serves its own [Bearer token realm](token-realm.md). `GET /v2/` challenges when an index
  restricts access, `GET /v2/token` mints a repository-scoped JWT, and resource routes enforce it, so `docker login`
  validates a credential. It runs the upstream's own handshake for you when it pulls through.

For the full standards map, see [standards](reference/standards.md).

## Configure and use OCI indexes

Configuration activates the OCI implementation through `ecosystem = "oci"`. Start with the
[getting-started tutorial](tutorials/getting-started.md) for one cached and one hosted index. Use
[run a container registry](guides/container-registry.md) for cached, hosted, and virtual roles under one route.
OCI-specific options belong under `[index.settings]`; see [index settings](reference/settings.md).

## Web UI

The OCI implementation labels searchable entities as images. An index card opens its repository list. A repository page
lists tags, and a tag page shows the resolved manifest, config, layer descriptors, digest, media type, and a copyable
pull command.

An image index lists its platform children. An image manifest lists config and layer blobs by digest and size. Tar layer
rows expose a **contents** link that lists files and previews bounded text chunks through the layer browser endpoint.

Each descriptor shows source and byte availability. The text distinguishes local bytes, upstream-only bytes, and
unavailable bytes without relying on color. Manifest and blob deletion controls follow the OCI distribution routes and
their trash recovery rules.

<figure class="screen">
  <img class="screen-light" src="/screens/oci-manifest-light.png"
       alt="An OCI manifest with a pull command, config digest, and layer table" loading="lazy">
  <img class="screen-dark" src="/screens/oci-manifest-dark.png"
       alt="An OCI manifest with a pull command, config digest, and layer table" loading="lazy">
  <figcaption>An OCI manifest with a pull command, config digest, and layer table</figcaption>
</figure>

## Operational checks

- Pull Docker Hub official images by their short name (`ubuntu`, `nginx`) through a routed proxy:
  [Docker Hub names and upstream auth](hub-names-and-auth.md)
- Repository-scoped digest reads and single-platform tag responses: [manifest serving](manifest-serving.md)
- Block a compromised manifest, config, or layer by digest without deleting its evidence:
  [revoked OCI content](revoked-content.md)
- Docker Hub cache comparison with [distribution](https://distribution.github.io/distribution/) and
  [zot](https://zotregistry.dev/): [OCI performance](performance.md)
- The full walkthrough: [run a container registry](guides/container-registry.md)
- Front a registry that is not Docker Hub: point `cached` at GHCR (`https://ghcr.io`), ECR, or an Artifactory `/v2/`.
- Serve trusted HTTPS so clients need no insecure flag: [configure TLS or ACME](@/core/configuration.md#tls).
