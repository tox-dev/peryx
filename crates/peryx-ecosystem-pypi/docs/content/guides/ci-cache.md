+++
title = "Cache packages for CI"
description = "Put peryx between CI runners and pypi.org so runners share downloaded wheels."
weight = 1
+++

CI jobs often rebuild environments and download the same wheels. Run peryx near the runners and point installers at it.
The first job warms the cache; later jobs install from local disk.

## Run peryx next to the runners

On the CI host, or as a service in the runner network:

```shell
peryx serve --host 0.0.0.0 --port 4433 --data-dir /var/lib/peryx
```

The data directory is the cache. Mount it on a persistent volume.

In [Kubernetes](https://kubernetes.io/) or [docker-compose](https://docs.docker.com/compose/), run one container with
the binary and data volume. peryx needs no database or sidecar.

## Point the installers at it

Set the index environment variable at the runner or organization level:

{% tabs(names="uv, pip, project file") %}

```shell
export UV_INDEX_URL=http://peryx.internal:4433/root/pypi/simple/
```

%%%

```shell
export PIP_INDEX_URL=http://peryx.internal:4433/root/pypi/simple/
```

%%%

```toml
# pyproject.toml, for uv-managed projects
[[tool.uv.index]]
url = "http://peryx.internal:4433/root/pypi/simple/"
default = true
```

{% end %}

Jobs that pass `--index-url` can use the same URL.

## Docker builds

Builds inside `docker build` do not see the host network by default. Either pass the index through a build argument:

```dockerfile
ARG PIP_INDEX_URL
RUN pip install -r requirements.txt
```

```shell
docker build --build-arg PIP_INDEX_URL=http://peryx.internal:4433/root/pypi/simple/ .
```

You can also run the build on a network where `peryx.internal` resolves by using `--network` with
[BuildKit](https://github.com/moby/buildkit). BuildKit cache mounts serve one machine; peryx shares artifacts across
machines, tags, and projects.

## Inspect usage and storage

Run several jobs, then inspect cache usage:

```shell
curl -s -u operator:"$OPERATOR_PASSWORD" 'http://peryx.internal:4433/+stats?index=root/pypi' | jq .totals
```

`downloads` and `bytes` count what peryx served; once the working set is warm, upstream traffic drops to page
revalidations (`refreshes`, mostly `304`s with no body). The [dashboard](@/core/web-ui.md) shows the same numbers with
per-project drill-down, and [`/metrics`](@/core/monitor.md) feeds [Prometheus](https://prometheus.io/).

The cache CLI reads the shared content store and scopes metadata commands by the PyPI route:

```shell
peryx cache size --data-dir /var/lib/peryx
peryx cache list --data-dir /var/lib/peryx --index root/pypi --stale
peryx cache fsck --data-dir /var/lib/peryx
```

Purge uses a plan first. Add `--yes` after checking its row counts:

```shell
peryx cache purge project --data-dir /var/lib/peryx --index root/pypi --project flask
peryx cache purge project --data-dir /var/lib/peryx --index root/pypi --project flask --yes
peryx cache purge orphaned-blobs --data-dir /var/lib/peryx
peryx cache purge orphaned-blobs --data-dir /var/lib/peryx --yes
```

Project purge removes metadata rows and leaves shared blobs in place. Orphaned-blob purge removes content with no
metadata reference. Rebuild the derived package search after a metadata restore with `peryx job reindex`.

## Cache behavior

- Wheels are immutable and content-addressed. Each wheel crosses the uplink once
  ([architecture](@/core/architecture.md)).
- Cold misses stream through at upstream speed ([measurements](@/core/performance.md)).
- During a pypi.org outage, peryx serves stale pages and artifacts from disk.
