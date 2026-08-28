+++
title = "Build a team registry"
description = "Combine a hosted team store with a cached Docker Hub index under one virtual route."
weight = 2
+++

Build a team registry with a cached [Docker Hub](https://hub.docker.com/) proxy, one hosted store, and a virtual index
that resolves team images before upstream images. Clients use one route for push and pull. It takes about fifteen
minutes and builds on [getting started](@/ecosystems/oci/tutorials/getting-started.md).

## Target configuration

A team publishes its own images and pulls public ones through a single cache. A team image must resolve before a
same-named Docker Hub image.

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

[[index.upstream]]
name = "primary"
url = "https://registry-1.docker.io"

[[index]] # hosted: the team's own images, push needs the token
name = "team"
route = "team"
ecosystem = "oci"
hosted = true

[[index.access_token]]
name = "upload"
secret = "team-secret"
actions = ["write", "delete"]

[[index]] # virtual: team images shadow the upstream, uploads land in team
name = "root-oci"
route = "root/oci"
ecosystem = "oci"
layers = ["team", "dockerhub"]
upload = "team"
```

`root-oci` names the virtual index at route `root/oci`. It resolves the hosted `team` store before the `dockerhub`
cache. Its `upload` key sends pushes to `team`. Clients read and write the virtual route.

## Start peryx

```shell
peryx serve --config peryx.toml
```

peryx is now listening on `127.0.0.1:4433`. `docker` and `podman` trust a
[loopback](@/ecosystems/oci/guides/local-transport.md) registry (`localhost`, `127.0.0.0/8`) over plain HTTP with no
configuration, so on the same host the commands below work as written. Over the network (or from Docker Desktop, whose
engine runs in a VM), a client demands HTTPS: give peryx a certificate ([serve HTTPS](@/core/operations/serve-https.md))
or set the client's insecure-registry option. `crane` and `podman` take a per-command flag; the snippets show it.

The dashboard at [http://127.0.0.1:4433/](http://127.0.0.1:4433/) shows one virtual-index card, `root-oci`, showing its
layer stack in resolution order with `team` on top of `dockerhub` and the upload target marked.

## Team push

Pushing needs a write-granting `[[index.access_token]]` on the hosted store; peryx accepts any username, and the token's
secret is the Basic-auth password. A teammate logs in, tags an image for the `root/oci` route, and pushes it. peryx
streams blobs into the content-addressed store and verifies each digest on commit. When the client finds the same layer
in another repository, it can mount the layer instead of uploading its bytes; peryx checks source pull access before it
links the target. Mount `team-secret` at `/run/secrets/peryx-token` so the login command reads it from standard input:

{% <tabs names="docker, podman, crane"> %}

```shell
docker login 127.0.0.1:4433 -u _ --password-stdin < /run/secrets/peryx-token
docker tag alpine 127.0.0.1:4433/root/oci/app:1.0
docker push 127.0.0.1:4433/root/oci/app:1.0
```

%%%

```shell
podman login --tls-verify=false 127.0.0.1:4433 -u _ --password-stdin < /run/secrets/peryx-token
podman tag alpine 127.0.0.1:4433/root/oci/app:1.0
podman push --tls-verify=false 127.0.0.1:4433/root/oci/app:1.0
```

%%%

```shell
crane auth login 127.0.0.1:4433 -u _ --password-stdin < /run/secrets/peryx-token
crane push --insecure app.tar 127.0.0.1:4433/root/oci/app:1.0
```

{% </tabs> %}

The push landed in `team` because the virtual index names it as the `upload` target. Ask the registry for the tags it
now holds:

```shell
curl -s http://127.0.0.1:4433/v2/root/oci/app/tags/list   # {"name":"root/oci/app","tags":["1.0"]}
```

## Pull from one route

Every teammate pulls `app` and any public image through the same `root/oci` route. A name the team published serves the
team's image; anything unpublished falls through to Docker Hub, is cached on first pull, and comes from disk after:

{% <tabs names="docker, podman, crane"> %}

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

{% </tabs> %}

## Verify name shadowing

The team's `app` resolves to the team's push on the `root/oci` route. The virtual index walks its members hosted-first,
so `team` answers for a name it holds before `dockerhub`. A later Docker Hub repository with the same name cannot
override the hosted member. This is [shadowing](@/core/repositories/indexes.md#shadowing), the dependency-confusion
defense, applied to containers.

## Next steps

- [Run a container registry](@/ecosystems/oci/guides/container-registry.md): the three roles in detail, plus deleting
  images you no longer want.
- [OCI performance](@/ecosystems/oci/performance.md): how peryx compares to distribution and zot as a Docker Hub cache.
