+++
title = "OCI"
description = "The container ecosystem: what cached, hosted, and virtual mean for OCI/Docker registries, the /v2/ distribution protocol, and docker/podman/crane config."
weight = 2
sort_by = "weight"
template = "section.html"
[extra]
logos = [ "logos/oci.svg"]
+++

OCI is the container-image ecosystem: the format of container images and the HTTP protocol clients such as Docker and
Podman use to pull and push them. An image is a small tree, not one file: a **manifest** (a JSON document listing an
image's parts), a **config** blob, and one or more **layer** blobs (the filesystem, gzip-compressed). Every part is a
**blob** addressed by the sha256 of its bytes; a mutable **tag** (`latest`, `1.25`) points at a manifest's digest.
velodex serves OCI over the [distribution spec](https://github.com/opencontainers/distribution-spec) that registries
(Docker Hub, GHCR, ECR, Artifactory) implement.

## How OCI concepts map to velodex

velodex describes every ecosystem with one neutral vocabulary; here is how the container terms you already know line up
with it. In OCI contexts velodex uses the container term; the neutral name is what the same idea is called across
ecosystems (see [the index model](@/core/indexes.md) and [glossary](@/core/glossary.md)).

| Container term         | velodex concept  | What it is                                                           |
| ---------------------- | ---------------- | -------------------------------------------------------------------- |
| registry               | index            | the endpoint a client points at; a cached index proxies one upstream |
| repository             | project          | one image name, like `library/alpine`                                |
| tag                    | version          | a mutable name (`latest`, `1.25`) pointing at a manifest digest      |
| image (manifest+blobs) | artifact         | what you pull: a manifest, a config blob, and layers, not one file   |
| layer / blob           | file             | one content-addressed piece, stored once and shared across images    |
| digest (`sha256:…`)    | content address  | the sha256 that names and verifies every stored object               |
| push                   | upload / publish | putting an image into a hosted index                                 |
| pull                   | download         | fetching an image through velodex                                    |
| pull-through cache     | cached (role)    | a read-through proxy of one upstream registry                        |

The role names (**cached**, **hosted**, **virtual**) and **shadowing** are velodex's own, the same in every ecosystem.

## The roles for OCI

The three [index roles](@/core/indexes.md) map onto OCI like this:

- **cached**: a read-through cache of an upstream registry. On a miss velodex pulls the manifest or blob from upstream
  (running the bearer-token handshake the registry requires), verifies its digest, stores it, and serves it; later pulls
  come from disk. Point one at Docker Hub, GHCR, or any `/v2/` registry.
- **hosted**: a store you push your own images to. Blobs stream into the content-addressed store and are verified on
  commit; manifests are kept byte-for-byte so their digest is stable. Pushing needs a token (below).
- **virtual**: an ordered stack of members served under one name, where your hosted images shadow same-named upstream
  ones: a pull of a name you have published serves your image, and anything you have not published falls through to the
  upstream. This is the [dependency-confusion defense](@/core/glossary.md#shadowing), applied to containers.

## The wire protocol

Container clients speak the **distribution spec** over a `/v2/` API. velodex serves it directly:

- `GET /v2/`: the version check every client pings first; velodex answers `200` with
  `Docker-Distribution-API-Version: registry/2.0`.
- **Manifests**: `GET|HEAD|PUT|DELETE /v2/<name>/manifests/<tag-or-digest>`. velodex keeps a manifest byte-for-byte and
  addresses it by the sha256 of those exact bytes, so the `Docker-Content-Digest` a client verifies matches.
- **Blobs**: `GET|HEAD|DELETE /v2/<name>/blobs/<digest>`, plus the upload dance
  (`POST`/`PATCH`/`PUT /v2/<name>/blobs/uploads/…`) for push. Blobs are content-addressed and deduplicate across every
  index, so a cross-repo mount is a digest check. Concurrent pulls of one uncached layer share a single upstream fetch.
- **Tags**: `GET /v2/<name>/tags/list`.
- **Token auth**: a `401` carries a `WWW-Authenticate: Bearer` challenge; velodex runs that handshake for you when it
  pulls through, and requires a Basic-auth token when you push.

For the full standards map, see [standards](@/ecosystems/oci/reference/standards.md).

## Set me up

OCI images are content-addressed and immutable, so `<name>` in `/v2/<name>/…` carries the index route as a prefix: an
index at route `dockerhub` proxying Docker Hub serves `library/alpine` as `dockerhub/library/alpine`. Configure a proxy
and a hosted store:

```toml
# velodex.toml
[[index]]
name = "dockerhub"
route = "dockerhub"
ecosystem = "oci"
cached = "https://registry-1.docker.io"

[[index]]
name = "images"
route = "images"
ecosystem = "oci"
hosted = true
upload_token = "<token>"
```

Assume velodex is then running at `127.0.0.1:4433`.

Docker and Podman trust a **loopback** registry (`localhost`, `127.0.0.0/8`) over plain HTTP with no configuration, so
on the same host it just works. Reaching velodex over the network, or from Docker Desktop's VM, needs either
[TLS](@/core/configuration.md#tls) (the production path: a real or ACME certificate, no client flag) or the client's
insecure-registry setting. `crane` and `podman` take a per-command flag; the snippets below show it.

### Pull

{% tabs(names="docker, podman, crane") %}

```shell
docker pull 127.0.0.1:4433/dockerhub/library/alpine:latest
```

%%%

```shell
podman pull --tls-verify=false 127.0.0.1:4433/dockerhub/library/alpine:latest
```

%%%

```shell
crane pull --insecure 127.0.0.1:4433/dockerhub/library/alpine:latest alpine.tar
```

{% end %}

### Push

Pushing needs a hosted index with an `upload_token`. velodex accepts any username; the token is the Basic-auth password.

{% tabs(names="docker, podman, crane") %}

```shell
docker login 127.0.0.1:4433 -u _ -p <token>
docker tag alpine 127.0.0.1:4433/images/alpine:latest
docker push 127.0.0.1:4433/images/alpine:latest
```

%%%

```shell
podman login --tls-verify=false 127.0.0.1:4433 -u _ -p <token>
podman push --tls-verify=false alpine 127.0.0.1:4433/images/alpine:latest
```

%%%

```shell
crane auth login 127.0.0.1:4433 -u _ -p <token>
crane push --insecure alpine.tar 127.0.0.1:4433/images/alpine:latest
```

{% end %}

## In practice

- How velodex compares to distribution and zot as a Docker Hub cache: [OCI performance](@/ecosystems/oci/performance.md)
- The full walkthrough: [run a container registry](@/ecosystems/oci/guides/container-registry.md)
- Front a registry that is not Docker Hub: point `cached` at GHCR (`https://ghcr.io`), ECR, or an Artifactory `/v2/`.
- Serve trusted HTTPS so clients need no insecure flag: [configure TLS or ACME](@/core/configuration.md#tls).
