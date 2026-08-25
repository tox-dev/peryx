+++
title = "Mirror Docker Hub official images"
description = "Pull Docker Hub official images through a routed proxy and configure library_prefix."
weight = 2
+++

Docker Hub stores its official images under the `library` namespace: `ubuntu` is `library/ubuntu`. A client pulling
through a peryx route sends the name a user typed, so `docker pull peryx.internal:4433/hub/ubuntu` reaches peryx as
`ubuntu`, and Hub answers `401` for a repository by that name. `[index.settings].library_prefix` rewrites the upstream
request.

## Cache Docker Hub

```toml
# peryx.toml
[[index]]
name = "hub"
route = "hub"
ecosystem = "oci"

[[index.upstream]]
name = "primary"
url = "https://registry-1.docker.io"

[index.settings]
library_prefix = "auto" # the default; shown here for clarity
```

`auto` prefixes a single-segment name with `library/` when the upstream host is Docker Hub, which is what this index
proxies. Pull short names and names with a namespace through the same route:

```shell
docker pull peryx.internal:4433/hub/ubuntu:24.04           # peryx asks Hub for library/ubuntu
docker pull peryx.internal:4433/hub/library/nginx:latest   # passed through as spelled
docker pull peryx.internal:4433/hub/grafana/grafana:latest # a user repository, passed through
```

The rewrite reaches Hub only. peryx caches, tags, lists, and serves the image under `hub/ubuntu`, so a pipeline that
names `peryx.internal:4433/hub/ubuntu:24.04` keeps naming it that.

## Pre-seed official images offline

`peryx mirror` pulls through the same rule, so a short name works there as well and lands in the store under that name:

```shell
peryx mirror sync hub --config peryx.toml --option 'images=["ubuntu:24.04","nginx:1.27"]'
```

Follow up with `peryx mirror verify` to confirm every manifest and blob is on disk before a run with the network off;
see [serve images air-gapped](@/ecosystems/oci/guides/air-gapped.md).

## Override `library_prefix`

Use `auto` for Docker Hub and other upstreams. It rewrites only when the upstream host identifies Docker Hub. Set an
explicit value for the following cases.

**Set `true`** when the upstream is a Hub-compatible mirror on a different host, so `auto` cannot recognize it. A
pull-through mirror of Hub, or a corporate registry that reproduces Hub's namespace layout, wants short names resolved
the way Hub resolves them:

```toml
[[index]]
name = "hub-mirror"
route = "hub"
ecosystem = "oci"

[[index.upstream]]
name = "primary"
url = "https://hub-mirror.internal"

[index.settings]
library_prefix = true
```

**Set `false`** when the upstream is Docker Hub but you want the name passed through verbatim: an index that only ever
serves names with a namespace, or a debugging session where you need to see the client's exact request. With `false`, a
pull of `hub/ubuntu` asks Hub for `ubuntu` and gets Hub's `401`.

Registry-mirror mode needs neither. When the Docker daemon lists peryx under `registry-mirrors`, it resolves `ubuntu` to
`library/ubuntu` before it calls the mirror, and a mirror index carries an empty route, so the full name arrives. See
[cache images for CI](@/ecosystems/oci/guides/ci-cache.md).

## Troubleshoot failed pulls

An upstream `401` surfaces as a `401` with the `UNAUTHORIZED` code and a message naming the upstream, rather than as a
missing manifest. On a Hub proxy, the repository name reached Hub without anonymous access. Check that `library_prefix`
is not `false`, and that a user repository is spelled with its namespace. On a private upstream it points at the index's
credentials. See [Docker Hub names and upstream auth](@/ecosystems/oci/hub-names-and-auth.md).

## Related

- The setting, value by value: [index settings](@/ecosystems/oci/reference/settings.md)
- A start-to-finish walkthrough: [pull a Docker Hub official image](@/ecosystems/oci/tutorials/hub-official-images.md)
- The full role walkthrough: [run a container registry](@/ecosystems/oci/guides/container-registry.md)
