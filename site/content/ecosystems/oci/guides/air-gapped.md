+++
title = "Serve images air-gapped"
description = "Run peryx as a container registry with no path to Docker Hub by pre-seeding a cache or carrying the data directory across the gap."
weight = 6
+++

A network with no route to [Docker Hub](https://hub.docker.com/) can pull public images through peryx after the images
land in peryx's content-addressed blob store. Pin a pre-seeded cache offline or carry its data directory across the gap.
Images your team pushes to a hosted store need no upstream.

## Pre-seed the cache on a connected machine

On a machine that can reach Docker Hub, run peryx with a cached proxy and mirror all images the air-gapped side needs.
`peryx mirror sync` pulls each image's manifest and all named blobs (following a manifest list into its per-platform
manifests), running the upstream bearer-token handshake and verifying each blob against its digest:

```toml
# peryx.toml on the connected machine
host = "127.0.0.1"
port = 4433
data_dir = "./peryx-data"

[[index]] # cached: read-through cache of Docker Hub
name = "dockerhub"
route = "dockerhub"
ecosystem = "oci"

[[index.upstream]]
name = "primary"
url = "https://registry-1.docker.io"
```

```shell
peryx mirror sync dockerhub \
  --option 'images=["library/alpine:latest","library/nginx:1.27","library/python:3.13-slim"]'
```

The command stores each manifest, config blob, and layer blob under `./peryx-data`, deduplicated by digest. Re-run it
when the image set changes; `peryx mirror verify dockerhub --option 'images=[…]'` checks that the store contains a
complete image. The command does not require a running server. It reads the config and writes the data directory.

## Pin the cache offline

Set `offline = true` on the cached index to block upstream requests. peryx serves cached content from disk and returns
an error for content that was not pre-seeded:

```toml
# peryx.toml on the air-gapped machine
host = "0.0.0.0"
port = 4433
data_dir = "./peryx-data"

[[index]]
name = "dockerhub"
route = "dockerhub"
ecosystem = "oci"
offline = true

[[index.upstream]]
name = "primary"
url = "https://registry-1.docker.io"
```

Use this configuration on a machine that had network access during the pre-seed. Keep the data directory, set the flag,
and continue pulling through the `dockerhub` route.

## Transfer a backup

For an air-gapped machine with no prior network access, move the store to it. Pre-seed on the connected machine, carry
`./peryx-data` (the blob store and its metadata) across the gap, and run peryx there. Use a backup to keep the copy
consistent:

```shell
# connected machine
peryx backup create --data-dir ./peryx-data ./peryx-backup
peryx backup verify ./peryx-backup

# air-gapped machine
peryx restore ./peryx-backup --data-dir ./peryx-data
peryx serve --config peryx.toml
```

The air-gapped machine's config declares the same cached index with `offline = true`. Peryx serves a pre-seeded image
from the copied store and returns an error for a cold miss without attempting a network request.

## Hosted images need no upstream

An image your team builds and pushes to a hosted index does not involve Docker Hub, so it works air-gapped without a
pre-seed. Declare a hosted store alongside the cache:

```toml
[[index]] # hosted: your own images, push needs the token
name = "team"
route = "team"
ecosystem = "oci"
hosted = true

[[index.access_token]]
name = "upload"
secret = "team-secret"
actions = ["write", "delete"]
```

Mount `team-secret` at `/run/secrets/peryx-token`, then push and pull it on the air-gapped side:

```shell
docker login 127.0.0.1:4433 -u _ --password-stdin < /run/secrets/peryx-token
docker tag my-app 127.0.0.1:4433/team/my-app:1.0
docker push 127.0.0.1:4433/team/my-app:1.0
docker pull 127.0.0.1:4433/team/my-app:1.0
```

`podman` and `crane` push the same way with their insecure-transport flags. To serve hosted images and pre-seeded
upstream ones under one route, front both with a virtual index; see
[build a team registry](@/ecosystems/oci/tutorials/team-registry.md).

## Checks

- `curl -u admin:"$ADMIN_PASSWORD" http://<host>:4433/+status | jq '.indexes[].upstream?.offline'` shows which cached
  indexes run offline; the index topology needs an administrator credential.
- A failed pull from an offline cached route identifies content missing from the store. Add the image to the pre-seed
  set and transfer a new backup.
