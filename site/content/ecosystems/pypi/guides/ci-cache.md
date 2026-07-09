+++
title = "Cache packages for CI"
description = "Put peryx between your runners and pypi.org: one environment variable, faster jobs, and one download per wheel instead of hundreds."
weight = 1
+++

CI rebuilds environments from scratch, so every job downloads the same wheels again. Run one peryx where your runners
live and point the installers at it; the first job warms the cache and the rest install from local disk.

## Run peryx next to the runners

On the CI host, or as a service in the runner network:

```shell
peryx serve --host 0.0.0.0 --port 4433 --data-dir /var/lib/peryx
```

The data directory is the cache; give it a persistent volume. Nothing else is stateful.

In Kubernetes or docker-compose, the same thing is one container with one volume. The image only needs the binary and
the data mount; there is no database or sidecar.

## Point the installers at it

Installers pick up the index from an environment variable, so most setups change zero pipeline files: set it once at the
runner or organization level:

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

Jobs that already pass `--index-url` explicitly keep working; the flag and the variable point at the same place.

## Docker builds

Builds inside `docker build` do not see the host network by default. Either pass the index through a build argument:

```dockerfile
ARG PIP_INDEX_URL
RUN pip install -r requirements.txt
```

```shell
docker build --build-arg PIP_INDEX_URL=http://peryx.internal:4433/root/pypi/simple/ .
```

or run the build on a network where `peryx.internal` resolves (`--network` with BuildKit). BuildKit's own cache mounts
still help per machine; peryx makes the cache shared across machines, tags, and projects.

## Verify it is working

Watch a couple of jobs, then check what the cache absorbed:

```shell
curl -s 'http://peryx.internal:4433/+stats?index=root/pypi' | jq .totals
```

`downloads` and `bytes` count what peryx served; once the working set is warm, upstream traffic drops to page
revalidations (`refreshes`, mostly `304`s with no body). The [dashboard](@/core/web-ui.md) shows the same numbers with
per-project drill-down, and [`/metrics`](@/core/monitor.md) feeds Prometheus.

## Why this works as well as it does

- Wheels are immutable and content-addressed: each crosses your uplink once, ever
  ([architecture](@/core/architecture.md)).
- Cold misses stream through at upstream speed, so the warm-up phase costs nothing extra
  ([measurements](@/core/performance.md)).
- A pypi.org outage stops being a build outage: pages serve stale, artifacts serve from disk.
