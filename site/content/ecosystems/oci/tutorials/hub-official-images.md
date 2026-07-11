+++
title = "Pull a Docker Hub official image"
description = "Stand up a cached Docker Hub index and pull an official image such as ubuntu by its short name through peryx's route."
weight = 3
+++

In this tutorial you point peryx at [Docker Hub](https://hub.docker.com/) and pull `ubuntu` through it, typing the same
short name you would type against Hub itself. It takes about five minutes and assumes a peryx binary; see
[installation](@/core/installation.md).

Docker Hub keeps its official images (`ubuntu`, `nginx`, `postgres`) in a namespace called `library`, so `ubuntu` is
really `library/ubuntu`. peryx resolves that for you, and this tutorial shows the pull working end to end.

## Configure a Docker Hub proxy

Write a config with one cached index, routed at `hub`:

```toml
# peryx.toml
[[index]]
name = "hub"
route = "hub"
ecosystem = "oci"
cached = "https://registry-1.docker.io"
```

Nothing else is needed. The `library_prefix` setting that resolves the short name defaults to `auto`, and `auto`
recognizes the Docker Hub upstream from the host in `cached`.

## Start peryx

```shell
peryx serve --config peryx.toml
```

peryx listens on `127.0.0.1:4433`. Leave it running and open a second terminal.

`docker` and `podman` trust a [loopback](@/core/glossary.md#loopback-http) registry over plain HTTP with no
configuration, so the commands below work as written on the same host. Over the network, serve
[TLS](@/core/serve-https.md) or set the client's insecure-registry option.

## Pull the image by its short name

{% tabs(names="docker, podman, crane") %}

```shell
docker pull 127.0.0.1:4433/hub/ubuntu:latest
```

%%%

```shell
podman pull --tls-verify=false 127.0.0.1:4433/hub/ubuntu:latest
```

%%%

```shell
crane pull --insecure 127.0.0.1:4433/hub/ubuntu:latest ubuntu.tar
```

{% end %}

peryx asks Docker Hub for `library/ubuntu`, runs Hub's bearer-token handshake for that repository, verifies each digest,
and caches every blob. The second pull of the same image comes from disk.

## Verify what peryx stored

The image is cached under the name you pulled, not the name peryx asked Hub for:

```shell
curl -s http://127.0.0.1:4433/v2/hub/ubuntu/tags/list   # {"name":"hub/ubuntu","tags":["latest"]}
```

Open [http://127.0.0.1:4433/](http://127.0.0.1:4433/) and the repository is listed as `ubuntu` on the `hub` index. The
`library/` namespace lives on the upstream request alone, so everything you and your clients touch stays `ubuntu`.

A fully qualified name works through the same route, and peryx passes it through untouched:

```shell
docker pull 127.0.0.1:4433/hub/library/nginx:latest   # no rewrite; the name already names its namespace
docker pull 127.0.0.1:4433/hub/grafana/grafana:latest # a user repository, also untouched
```

## Where next

- Mirror official images in a pipeline, and when to change `library_prefix`:
  [Docker Hub official images](@/ecosystems/oci/guides/hub-official-images.md).
- Every value of the setting and what it rewrites: [index settings](@/ecosystems/oci/reference/settings.md).
- Why Hub needs the namespace at all, and what an upstream `401` means:
  [Docker Hub names and upstream auth](@/ecosystems/oci/hub-names-and-auth.md).
