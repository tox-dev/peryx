+++
title = "From devpi"
description = "What devpi and peryx share, what devpi does that peryx does not, what peryx adds, and how to move across."
weight = 1
[extra]
logos = [ "logos/devpi.png"]
+++

[devpi](https://github.com/devpi/devpi) combines a caching pypi.org mirror with user-owned indexes, index inheritance, a
web UI, primary/replica replication, and a pluggy-based extension model. Its
[server guide](https://github.com/devpi/devpi/blob/main/doc/adminman/server.rst) documents SQLite as the default storage
and PostgreSQL through a plugin. peryx covers the corresponding cache and hosted-index flows in one process.

## Comparison against peryx

### Overlap

Both are read-through pypi.org mirrors that cache what they fetch and host private uploads. Their caching behavior
overlaps in these areas:

- **Read-through mirroring** of pypi.org, as covered by devpi's
  [mirror guide](https://github.com/devpi/devpi/blob/main/doc/quickstart-pypimirror.rst), cached on first use.
- **Private uploads** through the
  [devpi package workflow](https://github.com/devpi/devpi/blob/main/doc/userman/devpi_packages.rst), served from the
  same host as the cached index.
- **Composition**: devpi's [index inheritance](https://github.com/devpi/devpi/blob/main/doc/userman/devpi_indices.rst)
  (`bases`) maps onto peryx's [virtual indexes](@/core/repositories/indexes.md). The default PyPI mode unions distinct
  filenames; configure [project isolation](@/ecosystems/pypi/reference/policy.md#project-isolation) when migrating a
  private name boundary.
- **Yank and delete** of hosted files.
- **A web UI** for browsing packages (devpi-web; built into peryx at `/`).
- **Streaming artifact downloads**: devpi's
  [`FileStreamer`](https://github.com/devpi/devpi/blob/main/server/devpi_server/views.py) and peryx both tee a wheel to
  disk while the client reads it, and both address stored files by sha256.

### devpi-only behavior

Migrating to peryx changes these areas:

- **User-owned indexes.** devpi indexes belong to users and carry
  [`acl_upload`](https://github.com/devpi/devpi/blob/main/doc/userman/devpi_indices.rst). peryx indexes belong to
  configuration. Artifact clients use configured or managed scoped tokens; server users and external groups use role
  grants for management access.
- **Replication protocol.** [devpi replicas](https://github.com/devpi/devpi/blob/main/doc/adminman/replica.rst) consume
  the primary's changelog and relay mutations to the primary. peryx selects `dc` or `ha` through `[availability]` and
  uses its own journal, frontier, placement, and authority contracts. No devpi replication state migrates.
- **Promotion (`push`).** devpi can
  [push a release between indexes](https://github.com/devpi/devpi/blob/main/doc/userman/devpi_packages.rst). In peryx,
  publish the release to the destination index.

### peryx-only behavior

- **[PEP 658](https://peps.python.org/pep-0658/) metadata by default.** The current devpi server keeps core metadata
  experimental behind
  [`--enable-core-metadata`](https://github.com/devpi/devpi/blob/main/server/devpi_server/config.py). peryx serves it
  without a feature switch and [synthesizes it with HTTP byte-range reads](@/contributing/runtime-architecture.md) when
  an upstream lacks it, so resolution can beat the upstream once metadata is cached.
- **Correctness under a concurrent cold burst.** On the first parallel fetch of a project, devpi can evaluate the
  request against an as-yet-empty project list, return a `404`, and cache that "does not exist" for its 30-minute mirror
  window; [uv](https://docs.astral.sh/uv/) then fails the install. peryx single-flights concurrent misses onto one
  upstream fetch, so ten cold installs of the same project all succeed.
- **Built-in observability.** [Prometheus](https://prometheus.io/) metrics and per-file usage counters are part of the
  server, not plugin territory.
- **One executable.** The same binary serves indexes and contains the `none`, `dc`, and `ha` availability
  implementations. It needs no `devpi-init` step or external database.

### Performance vs peryx

The [benchmark suite](@/core/operations/performance.md) runs both servers from their published packages against the same
workload. Cold and warm installs through uv:

{{<bench file="install-uv" only="peryx,devpi" owner="pypi" />}}

The parallel-install workload is where the concurrency difference shows up: ten virtualenvs install the same project at
once, each with an empty client cache.

{{<bench file="parallel-install" only="peryx,devpi" owner="pypi" />}}

The request workload drives a swarm of resolvers reading full project pages:

{{<bench file="load" only="peryx,devpi" owner="pypi" />}}

## Migration procedure

devpi's mirror state does not migrate and does not need to: peryx's cache refills on first use. Only your uploaded
packages need a `twine upload` pass into the new hosted index. Map the commands and knobs across:

| devpi                                        | peryx                                                            |
| -------------------------------------------- | ---------------------------------------------------------------- |
| `devpi-init` then `devpi-server --port 3141` | `peryx serve`                                                    |
| `http://host:3141/{user}/{index}/+simple/`   | `http://host:4433/{route}/simple/`                               |
| `devpi index -c dev bases=root/pypi`         | Virtual index with `layers = ["dev-hosted", "pypi"]`             |
| `devpi login` and `devpi upload`             | `twine upload --repository-url http://host:4433/{route}/ dist/*` |
| `devpi remove pkg==1.0`                      | `DELETE /{route}/{project}/{version}/`                           |
| `volatile=False`                             | `volatile = false` on the hosted index                           |
| `mirror_whitelist`                           | Explicit `fallback_mode` and `protected_names` source policy     |
| `acl_upload`                                 | configured or managed scoped tokens with `write`                 |
| devpi-web plugin                             | Built in at `/`                                                  |
| primary/replica options                      | `[availability]` with `mode = "dc"` or `mode = "ha"`             |

## Gotchas

- **Artifact ACLs become scoped tokens.** Use configured access grants or managed scoped tokens for principals that must
  retain distinct read, write, or delete permissions. Role grants cover management operations, not artifact requests.
- **Replication configuration must be rebuilt.** Configure peryx membership and roles; do not copy devpi changelog or
  replica state.
