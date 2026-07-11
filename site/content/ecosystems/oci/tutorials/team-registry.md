+++
title = "Build a team registry"
description = "One hosted store a whole team pushes to, fronted by a virtual index so team images shadow Docker Hub."
weight = 2
+++

In this tutorial you build a registry a whole team shares: a cached proxy of [Docker Hub](https://hub.docker.com/), one
hosted store everyone pushes images to, and a virtual index that layers the team's images over the upstream so a name
your team publishes is never pulled from Docker Hub. You will point every client at one URL for both push and pull. It
takes about fifteen minutes and builds on [getting started](@/ecosystems/oci/tutorials/getting-started.md).

## The goal

A team publishes its own images and pulls public ones through a single cache. An image the team pushes must always win
over a same-named image on Docker Hub, even if someone registers that name upstream tomorrow.

## Write the topology

Container images are content-addressed, so `<name>` in `/v2/<name>/…` carries the index route as a prefix: an index at
route `dockerhub` proxying Docker Hub serves `library/alpine` as `dockerhub/library/alpine`. Save this as `peryx.toml`:

```toml
# peryx.toml
host = "127.0.0.1"
port = 4433
data_dir = "peryx-data"

[[index]] # cached: read-through cache of Docker Hub
name = "dockerhub"
route = "dockerhub"
ecosystem = "oci"
cached = "https://registry-1.docker.io"

[[index]] # hosted: the team's own images, push needs the token
name = "team"
route = "team"
ecosystem = "oci"
hosted = true
upload_token = "team-secret"

[[index]] # virtual: team images shadow the upstream, uploads land in team
name = "root/oci"
route = "root/oci"
ecosystem = "oci"
layers = ["team", "dockerhub"]
upload = "team"
```

Read it bottom-up: `root/oci` is a virtual index that serves the hosted `team` store first and the `dockerhub` cache
second, and its `upload` key sends pushes to `team`. Clients read and write that one route; the two building-block
indexes live inside it.

## Start peryx

```shell
peryx serve --config peryx.toml
```

peryx is now listening on `127.0.0.1:4433`. `docker` and `podman` trust a [loopback](@/core/glossary.md#loopback-http)
registry (`localhost`, `127.0.0.0/8`) over plain HTTP with no configuration, so on the same host the commands below work
as written. Over the network (or from Docker Desktop, whose engine runs in a VM), a client demands HTTPS: give peryx a
certificate ([serve HTTPS](@/core/serve-https.md)) or set the client's insecure-registry option. `crane` and `podman`
take a per-command flag; the snippets show it.

The dashboard at [http://127.0.0.1:4433/](http://127.0.0.1:4433/) draws the topology: one virtual-index card,
`root/oci`, showing its layer stack in resolution order with `team` on top of `dockerhub` and the upload target marked.

## A teammate pushes an image

Pushing needs the hosted store's `upload_token`; peryx accepts any username, and the token is the Basic-auth password. A
teammate logs in, tags an image for the `root/oci` route, and pushes it. Blobs stream into the content-addressed store
and are verified on commit:

{% tabs(names="docker, podman, crane") %}

```shell
docker login 127.0.0.1:4433 -u _ -p team-secret
docker tag alpine 127.0.0.1:4433/root/oci/app:1.0
docker push 127.0.0.1:4433/root/oci/app:1.0
```

%%%

```shell
podman login --tls-verify=false 127.0.0.1:4433 -u _ -p team-secret
podman tag alpine 127.0.0.1:4433/root/oci/app:1.0
podman push --tls-verify=false 127.0.0.1:4433/root/oci/app:1.0
```

%%%

```shell
crane auth login 127.0.0.1:4433 -u _ -p team-secret
crane push --insecure app.tar 127.0.0.1:4433/root/oci/app:1.0
```

{% end %}

The push landed in `team` because the virtual index names it as the `upload` target. Ask the registry for the tags it
now holds:

```shell
curl -s http://127.0.0.1:4433/v2/root/oci/app/tags/list   # {"name":"root/oci/app","tags":["1.0"]}
```

## Everyone pulls from one URL

Every teammate pulls `app` and any public image through the same `root/oci` route. A name the team published serves the
team's image; anything unpublished falls through to Docker Hub, is cached on first pull, and comes from disk after:

{% tabs(names="docker, podman, crane") %}

```shell
docker pull 127.0.0.1:4433/root/oci/app:1.0                  # the team's build
docker pull 127.0.0.1:4433/root/oci/library/nginx:latest     # falls through to Docker Hub
```

%%%

```shell
podman pull --tls-verify=false 127.0.0.1:4433/root/oci/app:1.0
podman pull --tls-verify=false 127.0.0.1:4433/root/oci/library/nginx:latest
```

%%%

```shell
crane pull --insecure 127.0.0.1:4433/root/oci/app:1.0 app.tar
crane pull --insecure 127.0.0.1:4433/root/oci/library/nginx:latest nginx.tar
```

{% end %}

## Watch shadowing defend the name

The team's `app` now resolves only to the team's push on the `root/oci` route. The virtual index walks its members
hosted-first, so `team` answers for a name it holds and `dockerhub` is never consulted for that name. If someone
registers `app` on Docker Hub tomorrow, nothing changes: the hosted layer answers first. This is
[shadowing](@/core/glossary.md#shadowing), the dependency-confusion defense, applied to containers.

## Where next

- [Run a container registry](@/ecosystems/oci/guides/container-registry.md): the three roles in detail, plus deleting
  images you no longer want.
- [OCI performance](@/ecosystems/oci/performance.md): how peryx compares to distribution and zot as a Docker Hub cache.
