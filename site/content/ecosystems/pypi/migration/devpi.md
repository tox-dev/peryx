+++
title = "From devpi"
description = "What devpi and peryx share, what devpi does that peryx does not, what peryx adds, and how to move across."
weight = 1
[extra]
logos = [ "logos/devpi.png"]
+++

[devpi](https://devpi.net/) is the long-standing Python answer to this problem: a caching pypi.org mirror plus
user-owned indexes with inheritance, a web UI, primary/replica replication, and a
[pluggy](https://pluggy.readthedocs.io/)-based plugin ecosystem. It runs as a
[Pyramid](https://docs.pylonsproject.org/projects/pyramid/) application on an embedded
[waitress](https://docs.pylonsproject.org/projects/waitress/) server, keeps its state in an
[SQLite](https://www.sqlite.org/) key-value store with release files on the filesystem, and expects an
[nginx](https://nginx.org/) and [supervisor](http://supervisord.org/) front for production (its own `devpi-gen-config`
generates those files). peryx covers the same read-through behavior in one process.

## Comparison against peryx

### Overlap

Both are read-through pypi.org mirrors that cache what they fetch and host private uploads. Their caching behavior
overlaps in these areas:

- **Read-through mirroring** of pypi.org (or any simple index), cached on first use.
- **Private uploads** over the [twine](https://twine.readthedocs.io/) API, served from the same host as the cached
  index.
- **Composition**: devpi's index inheritance (`bases`) maps onto peryx's
  [virtual indexes](@/core/repositories/indexes.md). The default PyPI mode unions distinct filenames; configure
  [project isolation](@/ecosystems/pypi/reference/policy.md#project-isolation) when migrating a private name boundary.
- **Yank and delete** of hosted files.
- **A web UI** for browsing packages (devpi-web; built into peryx at `/`).
- **Streaming artifact downloads**: devpi's `FileStreamer` and peryx both tee a wheel to disk while the client reads it,
  and both address stored files by sha256.

### devpi-only behavior

Migrating to peryx changes these areas:

- **User-owned indexes.** devpi indexes belong to users and carry `acl_upload`. peryx indexes belong to configuration.
  Per-index access tokens carry resource-scoped `read`, `write`, and `delete` grants; server users and external groups
  use the shared role model.
- **Replication protocol.** devpi replicas consume its changelog. peryx selects `dc` or `ha` through `[availability]`
  and uses its own journal, frontier, placement, and authority contracts. No devpi replication state migrates.
- **Promotion (`push`).** devpi can promote a release from one index to another server-side. In peryx that is a
  re-upload.
- **Runtime plugins.** devpi-ldap, devpi-lockdown, and related packages load through pluggy. peryx owners are linked
  into the binary and activated by index configuration; it does not load third-party code at startup.

### peryx-only behavior

- **[PEP 658](https://peps.python.org/pep-0658/) metadata by default.** devpi 6.x ships core-metadata as experimental,
  behind `--enable-core-metadata`. peryx serves it out of the box and
  [synthesizes it with HTTP byte-range reads](@/contributing/runtime-architecture.md) when an upstream lacks it, so
  resolution can beat the upstream once metadata is cached.
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
| `acl_upload`                                 | One or more scoped `[[index.access_token]]` grants               |
| devpi-web plugin                             | Built in at `/`                                                  |
| primary/replica options                      | `[availability]` with `mode = "dc"` or `mode = "ha"`             |

## Gotchas

- **ACLs move into configuration.** Create separate scoped access tokens or server-role grants for principals that must
  retain distinct permissions.
- **No `push` between indexes.** Promoting a release is a re-upload into the target index.
- **Pluggy hooks have no runtime counterpart.** Move custom hooks to a gateway or automation against the HTTP API.
- **Replication configuration must be rebuilt.** Configure peryx membership and roles; do not copy devpi changelog or
  replica state.
