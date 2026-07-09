+++
title = "Compose a virtual registry"
description = "Stack a hosted store over a cached Docker Hub proxy so your own images shadow same-named upstream ones, the dependency-confusion defense for containers."
weight = 2
+++

A virtual OCI index lists other indexes as `layers` and serves them under one route. Resolution walks the layers in
order and keeps the first member that holds an image, so a hosted member listed ahead of a proxy shadows any same-named
image upstream. This is the [shadowing](@/core/glossary.md#shadowing) rule, the dependency-confusion defense, applied to
containers: a pull of a name you published serves your image, and anything you have not published falls through to the
upstream. See [the index model](@/core/indexes.md) for the semantics.

## Configure the stack

Three indexes: a proxy that caches Docker Hub, a hosted store for your own images, and a virtual index that stacks them
hosted-first. Members and the virtual index must share the `oci` ecosystem.

```toml
# peryx.toml
host = "127.0.0.1"
port = 4433

[[index]] # proxy: cache Docker Hub
name = "dockerhub"
route = "dockerhub"
ecosystem = "oci"
cached = "https://registry-1.docker.io"

[[index]] # hosted: your own images
name = "images"
route = "images"
ecosystem = "oci"
hosted = true
upload_token = "<token>"

[[index]] # virtual: hosted shadows the proxy
name = "reg"
route = "reg"
ecosystem = "oci"
layers = ["images", "dockerhub"]
upload = "images"
```

Run it with `peryx serve --config peryx.toml`. Clients now read and write one route, `reg`.

`layers` order is the whole control: `images` before `dockerhub` means your images win. Reverse them and Docker Hub
shadows yours: dependency confusion, self-inflicted. `upload` names the hosted layer that receives pushes to the virtual
route; omit it and peryx picks the first hosted layer. A virtual index of only proxies rejects pushes.

## A note on transport

`docker` and `podman` trust a [loopback](@/core/glossary.md#loopback-http) registry (`localhost`, `127.0.0.0/8`) over
plain HTTP with no configuration, so on the same host the commands below work as written. Over the network (or from
Docker Desktop, whose engine runs in a VM where the host's `localhost` is not the engine's), a client demands HTTPS.
Give peryx a certificate ([serve HTTPS](@/core/serve-https.md)) or set the client's insecure-registry option. `crane`
and `podman` take a per-command flag, shown below; `docker` needs `insecure-registries` in its daemon config.

## Pull through the virtual route

A pull of `reg` walks the members hosted-first. The name decides the source:

```shell
# your build if you pushed `my-app` to the hosted layer, otherwise Docker Hub's:
docker pull 127.0.0.1:4433/reg/my-app:1.0
# always Docker Hub — you have not published nginx, so it falls through:
docker pull 127.0.0.1:4433/reg/library/nginx:latest
```

Once you push `my-app` to the hosted layer, the name is shadowed: every pull of `reg/my-app` serves your image, and a
same-named image appearing on Docker Hub can never take its place. Publishing privately is what turns a name off
upstream; there is no separate deny-list to maintain.

{% tabs(names="docker, podman, crane") %}

```shell
docker pull 127.0.0.1:4433/reg/my-app:1.0
```

%%%

```shell
podman pull --tls-verify=false 127.0.0.1:4433/reg/my-app:1.0
```

%%%

```shell
crane pull --insecure 127.0.0.1:4433/reg/my-app:1.0 my-app.tar
```

{% end %}

## Push into the stack

A push to `reg` lands in the layer named by `upload` (here `images`), so one route reads and writes. peryx accepts any
username; the token is the Basic-auth password. Blobs stream into the content-addressed store and are verified on
commit:

{% tabs(names="docker, podman, crane") %}

```shell
docker login 127.0.0.1:4433 -u _ -p <token>
docker tag my-app 127.0.0.1:4433/reg/my-app:1.0
docker push 127.0.0.1:4433/reg/my-app:1.0
```

%%%

```shell
podman login --tls-verify=false 127.0.0.1:4433 -u _ -p <token>
podman push --tls-verify=false my-app 127.0.0.1:4433/reg/my-app:1.0
```

%%%

```shell
crane auth login 127.0.0.1:4433 -u _ -p <token>
crane push --insecure my-app.tar 127.0.0.1:4433/reg/my-app:1.0
```

{% end %}

The pushed image is now visible on both routes: `reg/my-app` (through the stack) and `images/my-app` (the hosted store
directly). The proxy at `dockerhub` is untouched; shadowing is a resolution rule, not a copy.

## Failure behavior

A member that cannot answer (a down upstream with a cold cache) is skipped with a warning rather than failing the pull,
so images you host stay pullable during a Docker Hub outage. A proxy with a warm cache serves its cached copy instead.

## Related

- The role and shadowing model: [the index model](@/core/indexes.md)
- The full three-role walkthrough: [run a container registry](@/ecosystems/oci/guides/container-registry.md)
- The protocol and every client snippet: [OCI ecosystem](@/ecosystems/oci/_index.md)
- Serve trusted HTTPS so clients need no insecure flag: [serve HTTPS](@/core/serve-https.md)
