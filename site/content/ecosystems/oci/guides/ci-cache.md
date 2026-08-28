+++
title = "Cache images for CI"
description = "Cache Docker Hub base images for CI runners and reduce anonymous upstream pulls."
weight = 1
+++

Clean CI runners repeatedly pull the same base images. [Docker Hub](https://hub.docker.com/) caps anonymous pulls at 100
per six hours per IP, so runners behind one NAT egress can exhaust the quota. A peryx instance on the runner network
caches the first pull and serves later pulls from local disk.

## Run peryx next to the runners

On the CI host, or as a service in the runner network:

```shell
peryx serve --host 0.0.0.0 --port 4433 --data-dir /var/lib/peryx
```

Configure a proxy of Docker Hub. The `route` becomes the name prefix clients pull through:

```toml
# peryx.toml
[[index]]
name = "dockerhub"
route = "dockerhub"
ecosystem = "oci"

[[index.upstream]]
name = "primary"
url = "https://registry-1.docker.io"
```

The data directory is the cache; give it a persistent volume. peryx needs no external database or sidecar, so
[Kubernetes](https://kubernetes.io/) or [docker-compose](https://docs.docker.com/compose/) needs one container and one
volume.

## Transport: HTTP on loopback, TLS over the network

`docker` and `podman` trust a loopback registry (`localhost`, `127.0.0.0/8`) over plain HTTP with no configuration, so a
runner on the same host as peryx works as written. Reaching peryx across the runner network (the usual CI shape) means a
client demands HTTPS: give peryx a certificate ([serve HTTPS](@/core/operations/serve-https.md)) or set the client's
insecure-registry option. `crane` and `podman` take a per-command flag; `docker` needs an `insecure-registries` entry in
its daemon config. The examples use `peryx.internal:4433`.

## Pull through the proxy

Point the client at the route instead of Docker Hub. `alpine:latest` becomes
`peryx.internal:4433/dockerhub/library/alpine:latest`:

{% <tabs names="docker, podman, crane"> %}

```shell
docker pull peryx.internal:4433/dockerhub/library/alpine:latest
```

%%%

```shell
podman pull peryx.internal:4433/dockerhub/library/alpine:latest
```

%%%

```shell
crane pull peryx.internal:4433/dockerhub/library/alpine:latest alpine.tar
```

{% </tabs> %}

The first pull runs the upstream's Bearer-token handshake, verifies each digest, and caches the blobs; subsequent pulls
come from disk. Content addressing lets images that share a layer use one cached copy.

The daemon checks the registry with `GET /v2/`, then requests the manifest at
`GET /v2/dockerhub/library/alpine/manifests/latest`. peryx uses `dockerhub` to select the cached index and sends
`library/alpine` to Docker Hub.

## Rewrite images in a pipeline

Prefix the route wherever the pipeline names an image. For example, a [GitHub Actions](https://docs.github.com/actions)
job can use:

```yaml
jobs:
  test:
    runs-on: [self-hosted]
    steps:
      - run: docker pull peryx.internal:4433/dockerhub/library/postgres:16
      - run: docker run --rm peryx.internal:4433/dockerhub/library/postgres:16
```

## Keep the route in image names

Do not register this routed endpoint under Docker's `registry-mirrors`. For `docker pull alpine`, a root mirror receives
the manifest request as `GET /v2/library/alpine/manifests/latest`; Docker does not add the `dockerhub` route. peryx
requires every configured index to have a [non-empty route](@/core/operations/configuration.md), so the `dockerhub`
index cannot match that request.

Rewrite each image reference with the route as shown above. Images from [GHCR](https://docs.github.com/packages),
[ECR](https://aws.amazon.com/ecr/), or a private registry need their own cached index and route.

## Inspect usage and storage

Run two jobs, then check the cache statistics:

```shell
curl -s -u operator:"$OPERATOR_PASSWORD" 'http://peryx.internal:4433/+stats?index=dockerhub' | jq .totals
```

`downloads` and `bytes` count what peryx served. Once the working set is warm, upstream traffic drops to manifest
revalidations while layer bytes come from disk. [`/metrics`](@/core/operations/monitor.md) exports these counters to
[Prometheus](https://prometheus.io/).

The cache CLI reads the shared content store and scopes metadata commands by the OCI route:

```shell
peryx cache size --data-dir /var/lib/peryx
peryx cache list --data-dir /var/lib/peryx --index dockerhub --stale
peryx cache fsck --data-dir /var/lib/peryx
```

Purge uses a plan first. Add `--yes` after checking its row counts:

```shell
peryx cache purge project --data-dir /var/lib/peryx --index dockerhub --project library/alpine
peryx cache purge project --data-dir /var/lib/peryx --index dockerhub --project library/alpine --yes
peryx cache purge orphaned-blobs --data-dir /var/lib/peryx
peryx cache purge orphaned-blobs --data-dir /var/lib/peryx --yes
```

Project purge removes repository metadata and leaves shared blobs in place. Orphaned-blob purge removes content with no
metadata reference. Rebuild the derived image search after a metadata restore with `peryx job reindex`.

## Cache behavior

- Blobs are immutable and content-addressed. Each layer uses one stored copy across images and tags.
- Concurrent pulls of one uncached layer collapse to a single upstream fetch, so a fan-out of parallel jobs does not
  multiply the miss.
- After warm-up, peryx serves cached layers to the fleet and Docker Hub receives manifest revalidations instead of cold
  layer pulls.
- Docker Hub cache comparison with [distribution](https://distribution.github.io/distribution/) and
  [zot](https://zotregistry.dev/): [OCI performance](@/ecosystems/oci/performance.md).
- The full role walkthrough, hosted and virtual as well as proxy:
  [run a container registry](@/ecosystems/oci/guides/container-registry.md).
