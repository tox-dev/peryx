+++
title = "Getting started"
description = "Serve container images through peryx: cache Docker Hub, pull an image, push one of your own, and verify it."
weight = 1
+++

In this tutorial you configure peryx as a container registry, start it, pull an image through a cached proxy of
[Docker Hub](https://hub.docker.com/), then build and push an image of your own to a private hosted store. It takes
about ten minutes.

An OCI image is a small tree, not one file: a **manifest** (a JSON document listing the image's parts), a config blob,
and one or more **layer** blobs. Each part is a **blob** addressed by the sha256 of its bytes, and a mutable **tag**
(`latest`, `1.0`) points at a manifest. peryx serves all of this over the container `/v2/` protocol that `docker`,
`podman`, and `crane` speak.

## Prerequisites

You need a container client (`docker`, `podman`, or [`crane`](https://github.com/google/go-containerregistry)) and a
peryx binary. Pick whichever install channel fits; [installation](@/core/installation.md) lists them all:

{% tabs(names="installer, from source") %}

```shell
# standalone binary, no toolchain involved
curl -LsSf https://github.com/tox-dev/peryx/releases/latest/download/peryx-installer.sh | sh
```

%%%

```shell
# needs a Rust toolchain (https://rustup.rs); rust-toolchain.toml pins the version
git clone https://github.com/tox-dev/peryx.git
cd peryx
cargo build --release
```

{% end %}

## Configure peryx

Container images are content-addressed and immutable, so `<name>` in `/v2/<name>/…` carries the index route as a prefix:
an index at route `dockerhub` proxying Docker Hub serves `library/alpine` as `dockerhub/library/alpine`. Write a config
with two indexes: a cached proxy of Docker Hub, and a hosted store for your own images:

```toml
# peryx.toml
[[index]] # cached: read-through cache of Docker Hub
name = "dockerhub"
route = "dockerhub"
ecosystem = "oci"
cached = "https://registry-1.docker.io"

[[index]] # hosted: your own images, push needs the token
name = "images"
route = "images"
ecosystem = "oci"
hosted = true
upload_token = "demo-secret"
```

## Start peryx

Start the server with that config:

```shell
peryx serve --config peryx.toml   # ./target/release/peryx serve when built from source
```

peryx is now listening on `127.0.0.1:4433`. Leave it running and use a second terminal for the rest of the tutorial.

`docker` and `podman` trust a **loopback** registry (`localhost`, `127.0.0.0/8`) over plain HTTP with no configuration,
so on the same host the commands below work as written. Reaching peryx over the network (or from Docker Desktop, whose
engine runs in a VM) needs either [TLS](@/core/configuration.md#tls) (the production path, no client flag) or the
client's insecure-registry setting. `crane` and `podman` take a per-command flag; the snippets show it.

## Pull through the cache

Pull `library/alpine` through the `dockerhub` route. The first pull runs Docker Hub's bearer-token handshake, verifies
each blob against its digest, and caches it; later pulls come from disk:

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

## Push your own image

Pushing needs the hosted index's `upload_token`. peryx accepts any username; the token is the Basic-auth password. Log
in, tag an image for the `images` route, and push it. Blobs stream into the content-addressed store and are verified on
commit:

{% tabs(names="docker, podman, crane") %}

```shell
docker login 127.0.0.1:4433 -u _ -p demo-secret
docker tag alpine 127.0.0.1:4433/images/app:1.0
docker push 127.0.0.1:4433/images/app:1.0
```

%%%

```shell
podman login --tls-verify=false 127.0.0.1:4433 -u _ -p demo-secret
podman tag alpine 127.0.0.1:4433/images/app:1.0
podman push --tls-verify=false 127.0.0.1:4433/images/app:1.0
```

%%%

```shell
crane auth login 127.0.0.1:4433 -u _ -p demo-secret
crane push --insecure alpine.tar 127.0.0.1:4433/images/app:1.0
```

{% end %}

## Verify

Pull your image back from a clean state to confirm it round-trips. Ask the registry for the tags it holds, then pull:

```shell
curl -s http://127.0.0.1:4433/v2/images/app/tags/list   # {"name":"images/app","tags":["1.0"]}
docker pull 127.0.0.1:4433/images/app:1.0
```

peryx also serves a web interface on the same port. Open [http://127.0.0.1:4433/](http://127.0.0.1:4433/) for a live
dashboard of the configured indexes and their request counters; the same numbers are
[Prometheus](https://prometheus.io/) metrics at `/metrics`.

## Where next

- [Run a container registry](@/ecosystems/oci/guides/container-registry.md): add a virtual index so your hosted images
  shadow upstream, and delete images you no longer want.
- [OCI performance](@/ecosystems/oci/performance.md): how peryx compares to distribution and zot as a Docker Hub cache.
- [Configuration reference](@/core/configuration.md): every TOML key, including TLS so clients need no insecure flag.
