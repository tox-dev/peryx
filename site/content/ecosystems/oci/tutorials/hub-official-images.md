+++
title = "Pull a Docker Hub official image"
description = "Configure a Docker Hub cache and pull an official image by its short name."
weight = 3
+++

Configure a cached [Docker Hub](https://hub.docker.com/) index and pull `ubuntu` through it by its short name. Allow
five minutes after [installing peryx](@/core/start/installation.md).

Docker Hub stores official images such as `ubuntu` under the `library` namespace. peryx resolves `ubuntu` to
`library/ubuntu` before sending the upstream request.

## Configure a Docker Hub proxy

Write a config with one cached index, routed at `hub`:

```toml
# peryx.toml
[[index]]
name = "hub"
route = "hub"
ecosystem = "oci"

[[index.upstream]]
name = "primary"
url = "https://registry-1.docker.io"
```

`[index.settings].library_prefix` defaults to `"auto"`; it recognizes Docker Hub from the upstream host.

## Start peryx

```shell
peryx serve --config peryx.toml
```

peryx listens on `127.0.0.1:4433`. Leave it running and open a second terminal.

`docker` and `podman` trust a [loopback](@/ecosystems/oci/guides/local-transport.md) registry over plain HTTP with no
configuration, so the commands below work as written on the same host. Over the network, serve
[TLS](@/core/operations/serve-https.md) or set the client's insecure-registry option.

## Pull the image by its short name

{% <tabs names="docker, podman, crane"> %}

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

{% </tabs> %}

peryx asks Docker Hub for `library/ubuntu`, runs Hub's bearer-token handshake for that repository, verifies each digest,
and caches every blob. The second pull of the same image comes from disk.

## Verify cached state

The cache uses the client-facing name:

```shell
curl -s http://127.0.0.1:4433/v2/hub/ubuntu/tags/list   # {"name":"hub/ubuntu","tags":["latest"]}
```

Open [http://127.0.0.1:4433/](http://127.0.0.1:4433/) and the repository is listed as `ubuntu` on the `hub` index. The
`library/` namespace applies only to the upstream request, so client-facing routes keep `ubuntu`.

A fully qualified name works through the same route, and peryx passes it through untouched:

```shell
docker pull 127.0.0.1:4433/hub/library/nginx:latest   # no rewrite; the name already names its namespace
docker pull 127.0.0.1:4433/hub/grafana/grafana:latest # a user repository, also untouched
```

## Related

- Mirror official images in a pipeline, and when to change `library_prefix`:
  [Docker Hub official images](@/ecosystems/oci/guides/hub-official-images.md).
- Every value of the setting and what it rewrites: [index settings](@/ecosystems/oci/reference/settings.md).
- Why Hub needs the namespace at all, and what an upstream `401` means:
  [Docker Hub names and upstream auth](@/ecosystems/oci/hub-names-and-auth.md).
