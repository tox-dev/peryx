+++
title = "Serve images air-gapped"
description = "Run velodex as a container registry on a network with no path to Docker Hub: pre-seed a cache, then run offline or carry the data directory across the gap."
weight = 6
+++

A network with no route to Docker Hub can still pull public images through velodex, as long as the images land in
velodex's content-addressed blob store before the gap closes. Two topologies cover the common cases: a cache pinned
offline, or a data directory carried across. Images your team pushes directly to a hosted store need no upstream at all.

## Pre-seed the cache on a connected machine

On a machine that can still reach Docker Hub, run velodex with a cached proxy and mirror every image the air-gapped side
will need. `velodex mirror sync` pulls each image's manifest and every blob it names (following a manifest list into its
per-platform manifests), running the upstream bearer-token handshake and verifying each blob against its digest:

```toml
# velodex.toml on the connected machine
host = "127.0.0.1"
port = 4433
data_dir = "./velodex-data"

[[index]] # cached: read-through cache of Docker Hub
name = "dockerhub"
route = "dockerhub"
ecosystem = "oci"
cached = "https://registry-1.docker.io"
```

```shell
velodex mirror sync dockerhub \
  --image library/alpine:latest \
  --image library/nginx:1.27 \
  --image library/python:3.13-slim
```

Every manifest, config blob, and layer blob now sits under `./velodex-data`, deduplicated by digest. Re-run the command
whenever the image set changes; `velodex mirror verify dockerhub --image …` checks that the store still holds a complete
image. A running server is not required; the command reads the config and writes the data directory directly.

## Approach one: pin the cache offline

Set `offline = true` on the cached index and velodex never reaches upstream. Everything already in the blob store serves
from disk; a pull of something that was not pre-seeded returns an error rather than a network fetch:

```toml
# velodex.toml on the air-gapped machine
host = "0.0.0.0"
port = 4433
data_dir = "./velodex-data"

[[index]]
name = "dockerhub"
route = "dockerhub"
ecosystem = "oci"
cached = "https://registry-1.docker.io"
offline = true
```

This fits a machine that was connected during the pre-seed and later lost its route: same data directory, one flag
flipped. Clients pull through the `dockerhub` route exactly as before.

## Approach two: carry the data directory across the gap

When the air-gapped machine was never connected, move the store to it. Pre-seed on the connected machine as above, then
carry `./velodex-data` (the blob store and its metadata) across the gap and run velodex there. A backup keeps the copy
consistent:

```shell
# connected machine
velodex backup create --data-dir ./velodex-data ./velodex-backup
velodex backup verify ./velodex-backup

# air-gapped machine
velodex restore ./velodex-backup --data-dir ./velodex-data
velodex serve --config velodex.toml
```

The air-gapped machine's config declares the same cached index with `offline = true`, so a pull of a pre-seeded image
serves from the carried-over store and a cold miss returns an error instead of reaching for a network that is not there.

## Hosted images need no upstream

An image your team builds and pushes to a hosted index never involves Docker Hub, so it works air-gapped with no
pre-seed at all. Declare a hosted store alongside the cache:

```toml
[[index]] # hosted: your own images, push needs the token
name = "team"
route = "team"
ecosystem = "oci"
hosted = true
upload_token = "team-secret"
```

Push and pull it directly on the air-gapped side:

```shell
docker login 127.0.0.1:4433 -u _ -p team-secret
docker tag my-app 127.0.0.1:4433/team/my-app:1.0
docker push 127.0.0.1:4433/team/my-app:1.0
docker pull 127.0.0.1:4433/team/my-app:1.0
```

`podman` and `crane` push the same way with their insecure-transport flags. To serve hosted images and pre-seeded
upstream ones under one route, front both with a virtual index; see
[build a team registry](@/ecosystems/oci/tutorials/team-registry.md).

## What to check

- `curl http://<host>:4433/+status` lists the indexes and their counters.
- `curl http://<host>:4433/+status | jq '.indexes[].upstream?.offline'` shows which cached indexes run offline.
- A pull that errors from a cached route means a client asked for an image the offline store does not hold; add it to
  the pre-seed loop and repeat the carry.
