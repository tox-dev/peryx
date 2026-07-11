+++
title = "Cache images for CI"
description = "Put peryx between your runners and Docker Hub: pull base images through one cache, and stop hitting the anonymous pull-rate limit."
weight = 1
+++

CI jobs start from clean runners, so every build and test job pulls the same base images again.
[Docker Hub](https://hub.docker.com/) caps anonymous pulls at 100 per six hours per IP, and a busy fleet behind one NAT
egress burns through that in minutes. Run one peryx where your runners live, point their container clients at it, and
the first job warms the cache while the rest pull layers from local disk.

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
cached = "https://registry-1.docker.io"
```

The data directory is the cache; give it a persistent volume. Nothing else is stateful (no database, no sidecar), so in
[Kubernetes](https://kubernetes.io/) or [docker-compose](https://docs.docker.com/compose/) this is one container with
one volume.

## Transport: HTTP on loopback, TLS over the network

`docker` and `podman` trust a loopback registry (`localhost`, `127.0.0.0/8`) over plain HTTP with no configuration, so a
runner on the same host as peryx works as written. Reaching peryx across the runner network (the usual CI shape) means a
client demands HTTPS: give peryx a certificate ([serve HTTPS](@/core/serve-https.md), the production path) or set the
client's insecure-registry option. `crane` and `podman` take a per-command flag; `docker` needs an `insecure-registries`
entry in its daemon config. The rest of this guide assumes peryx answers at `peryx.internal:4433`.

## Pull through the proxy

Point the client at the route instead of Docker Hub. `alpine:latest` becomes
`peryx.internal:4433/dockerhub/library/alpine:latest`:

{% tabs(names="docker, podman, crane") %}

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

{% end %}

The first pull runs the upstream's bearer-token handshake, verifies each digest, and caches every blob; later pulls come
from disk. Because blobs are content-addressed, a layer shared across images crosses your uplink once, ever.

## Rewrite images in a pipeline

Most pipelines name images inline, so the direct form is to prefix the route in the pull step. A
[GitHub Actions](https://docs.github.com/actions) job:

```yaml
jobs:
  test:
    runs-on: [self-hosted]
    steps:
      - run: docker pull peryx.internal:4433/dockerhub/library/postgres:16
      - run: docker run --rm peryx.internal:4433/dockerhub/library/postgres:16
```

## Or mirror Docker Hub transparently

To leave every `docker pull alpine` unchanged, register peryx as a Docker Hub mirror in the daemon config. The daemon
then routes Docker Hub pulls through peryx without any prefix in the image name:

```json
{
  "registry-mirrors": [
    "https://peryx.internal:4433"
  ]
}
```

Reload the daemon (`systemctl reload docker`) and bake this `daemon.json` into your runner image. Note the `https://`:
the mirror endpoint must be TLS, so peryx needs a trusted certificate. If it serves plain HTTP, add its host to
`insecure-registries` in the same file:

```json
{
  "registry-mirrors": [
    "http://peryx.internal:4433"
  ],
  "insecure-registries": [
    "peryx.internal:4433"
  ]
}
```

In this mode the daemon resolves `alpine` to `library/alpine` before it calls the mirror, so peryx receives the full
name. A routed pull (`peryx.internal:4433/dockerhub/alpine`) arrives as the short name the user typed, and the cached
index resolves it; see [mirror Docker Hub official images](@/ecosystems/oci/guides/hub-official-images.md).

`registry-mirrors` covers Docker Hub only; images from [GHCR](https://docs.github.com/packages),
[ECR](https://aws.amazon.com/ecr/), or a private registry still resolve directly. For those, front each with its own
proxy index (point `cached` at `https://ghcr.io` and pull through that route) and rewrite the image reference.

## Verify it is working

Watch a couple of jobs, then check what the cache absorbed:

```shell
curl -s 'http://peryx.internal:4433/+stats?index=dockerhub' | jq .totals
```

`downloads` and `bytes` count what peryx served; once the working set is warm, upstream traffic drops to manifest
revalidations while layer bytes come from disk. [`/metrics`](@/core/monitor.md) feeds the same numbers to
[Prometheus](https://prometheus.io/).

## Why this works as well as it does

- Blobs are immutable and content-addressed: each layer crosses your uplink once, and deduplicates across every image
  and tag that shares it.
- Concurrent pulls of one uncached layer collapse to a single upstream fetch, so a fan-out of parallel jobs does not
  multiply the miss.
- The anonymous pull-rate limit stops being the wall: after warm-up, peryx serves the fleet and Docker Hub sees
  revalidations, not a hundred cold pulls an hour.
- How peryx compares to [distribution](https://distribution.github.io/distribution/) and [zot](https://zotregistry.dev/)
  as a Docker Hub cache: [OCI performance](@/ecosystems/oci/performance.md).
- The full role walkthrough, hosted and virtual as well as proxy:
  [run a container registry](@/ecosystems/oci/guides/container-registry.md).
