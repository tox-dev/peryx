+++
title = "Run a container registry"
description = "Proxy Docker Hub, host your own images, and shadow upstream with a virtual index, then pull and push with docker, podman, or crane."
weight = 11
+++

peryx speaks the [OCI distribution protocol](@/ecosystems/oci/_index.md), so `docker`, `podman`, and `crane` pull and
push against it the same way they do against Docker Hub or GHCR. This guide sets up the three roles for containers and
points a client at each. It assumes a built peryx; see [Getting started](@/core/getting-started.md).

## Configure the indexes

An [index](@/core/glossary.md#index) is one of three [roles](@/core/glossary.md#roles) paired with the `oci`
[ecosystem](@/core/glossary.md#ecosystem). This config declares all three: a proxy of Docker Hub, a hosted store for
your own images, and a virtual index that stacks them:

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
```

Run it with `peryx serve --config peryx.toml`.

## A note on transport

`docker` and `podman` trust a [loopback](@/core/glossary.md#loopback-http) registry (`localhost`, `127.0.0.0/8`) over
plain HTTP with no configuration, so on the same host the commands below work as written. Over the network (or from
Docker Desktop, whose engine runs in a VM where the host's `localhost` is not the engine's), a client demands HTTPS. For
that, give peryx a certificate ([serve HTTPS](@/core/serve-https.md)) or set the client's insecure-registry option.
`crane` and `podman` take a per-command flag, shown below; `docker` needs `insecure-registries` in its daemon config.

## Pull through the proxy

A pull of `library/alpine` through the `dockerhub` route runs the upstream's bearer-token handshake, verifies the
digest, caches every blob, and serves later pulls from disk:

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

## Push your own images

Pushing needs the hosted index's `upload_token`; peryx accepts any username, and the token is the Basic-auth password.
Blobs stream into the content-addressed store and are verified on commit:

{% tabs(names="docker, podman, crane") %}

```shell
docker login 127.0.0.1:4433 -u _ -p <token>
docker tag my-app 127.0.0.1:4433/images/my-app:1.0
docker push 127.0.0.1:4433/images/my-app:1.0
```

%%%

```shell
podman login --tls-verify=false 127.0.0.1:4433 -u _ -p <token>
podman push --tls-verify=false my-app 127.0.0.1:4433/images/my-app:1.0
```

%%%

```shell
crane auth login 127.0.0.1:4433 -u _ -p <token>
crane push --insecure my-app.tar 127.0.0.1:4433/images/my-app:1.0
```

{% end %}

The `upload_token` gates writes only. Reads are open: anyone who can reach the route can pull an image you pushed, so
restrict who reaches peryx at the network layer (or front it with TLS) when a hosted index holds private images.

## Combine both with a virtual index

Pull through the `reg` route and peryx walks the members hosted-first: an image you pushed to `images` wins over a
same-named one on Docker Hub, and anything you have not published falls through to the upstream. This is
[shadowing](@/core/glossary.md#shadowing), the dependency-confusion defense, applied to containers:

```shell
# your own build of `my-app` if you pushed it, otherwise Docker Hub's:
docker pull 127.0.0.1:4433/reg/my-app:1.0
# always Docker Hub (you have not published nginx):
docker pull 127.0.0.1:4433/reg/library/nginx:latest
```

A push to `reg` lands in the hosted layer, so clients read and write one route.

## Delete an image

A hosted index with `volatile = true` (the default) accepts deletes. `crane delete` removes a manifest by digest; peryx
answers `202` and later pulls of that digest return `404`:

```shell
crane delete --insecure 127.0.0.1:4433/images/my-app@sha256:<digest>
```

## Related

- The protocol, roles, and every client snippet: [OCI ecosystem](@/ecosystems/oci/_index.md)
- Serve trusted HTTPS so clients need no insecure flag: [serve HTTPS](@/core/serve-https.md)
- What ships per ecosystem: [capability matrix](@/core/capabilities.md)
