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
[SQLite](https://www.sqlite.org/) key-value store with release files on the filesystem, and expects an nginx and
supervisor front for production (its own `devpi-gen-config` generates those files). peryx covers the same read-through
core in one static Rust binary.

## Comparison against peryx

### Overlap

Both are read-through pypi.org mirrors that cache what they fetch, host private uploads, and let hosted names shadow the
cached index. For a caching mirror the two overlap almost completely:

- **Read-through mirroring** of pypi.org (or any simple index), cached on first use.
- **Private uploads** over the twine API, served from the same host as the cached index.
- **Composition**: devpi's index inheritance (`bases`) maps onto peryx's [virtual indexes](@/core/indexes.md), and
  hosted files shadow upstream ones in both.
- **Yank and delete** of hosted files.
- **A web UI** for browsing packages (devpi-web; built into peryx at `/`).
- **Streaming artifact downloads**: devpi's `FileStreamer` and peryx both tee a wheel to disk while the client reads
  it, and both address stored files by sha256.

### Extra: what devpi does that peryx does not

Migrating to peryx means giving these up:

- **Users and per-index ACLs.** devpi indexes belong to users, each with its own `acl_upload`. peryx has one upload
  token per hosted index and open reads.
- **Replication.** devpi's primary/replica protocol streams a changelog to read-only replicas. peryx has no
  equivalent; you run one instance per site and let each warm itself.
- **Promotion (`push`).** devpi can promote a release from one index to another server-side. In peryx that is a
  re-upload.
- **A plugin ecosystem.** devpi-ldap, devpi-lockdown, and friends hook devpi through pluggy. peryx's extension points
  are its HTTP API and its configuration, nothing loadable.

### Missing: what peryx adds

- **[PEP 658](https://peps.python.org/pep-0658/) metadata by default.** devpi 6.x ships core-metadata as experimental,
  behind `--enable-core-metadata`. peryx serves it out of the box and
  [synthesizes it with HTTP byte-range reads](@/core/architecture.md) when an upstream lacks it, so resolution can beat
  the upstream once metadata is cached.
- **Correctness under a concurrent cold burst.** On the first parallel fetch of a project, devpi can evaluate the
  request against an as-yet-empty project list, return a `404`, and cache that "does not exist" for its 30-minute mirror
  window; uv then fails the install. peryx single-flights concurrent misses onto one upstream fetch, so ten cold
  installs of the same project all succeed.
- **Built-in observability.** Prometheus metrics and per-file usage counters are part of the server, not plugin
  territory.
- **One process.** A single static binary with no nginx or supervisor front, no `devpi-init` step, and no external
  database.

### Performance vs peryx

The [benchmark suite](@/core/performance.md) runs both servers from their published packages against the same workload.
Cold and warm installs through uv:

{{ bench(file="install-uv", only="peryx,devpi") }}

The parallel-install workload is where the concurrency difference shows up: ten virtualenvs install the same project at
once, each with an empty client cache.

{{ bench(file="parallel-install", only="peryx,devpi") }}

The request workload drives a swarm of resolvers reading full project pages:

{{ bench(file="load", only="peryx,devpi") }}

## How to migrate

devpi's mirror state does not migrate and does not need to: peryx's cache refills on first use. Only your uploaded
packages need a `twine upload` pass into the new hosted index. Map the commands and knobs across:

| devpi                                        | peryx                                                                                                     |
| -------------------------------------------- | ----------------------------------------------------------------------------------------------------------- |
| `devpi-init` then `devpi-server --port 3141` | `peryx serve` (no init step)                                                                              |
| `http://host:3141/{user}/{index}/+simple/`   | `http://host:4433/{route}/simple/`                                                                          |
| `devpi index -c dev bases=root/pypi`         | a virtual index with `layers = ["dev-hosted", "pypi"]` in [TOML](@/core/configuration.md)                   |
| `devpi login` + `devpi upload`               | `twine upload --repository-url http://host:4433/{route}/ dist/*` (any username, `upload_token` as password) |
| `devpi remove pkg==1.0`                      | `DELETE /{route}/{project}/{version}/` ([removal guide](@/ecosystems/pypi/guides/remove.md))                |
| `volatile=False`                             | `volatile = false` on the hosted index                                                                      |
| `mirror_whitelist`                           | not needed: hosted names shadow the cached index by default ([why](@/core/indexes.md))                      |
| `acl_upload`                                 | one `upload_token` per hosted index                                                                         |
| devpi-web plugin                             | built in at `/`                                                                                             |

## Gotchas

- **One upload token replaces per-person write control.** If you relied on distinct `acl_upload` per user, issue a
  distinct hosted index (and token) per team instead.
- **No `push` between indexes.** Promoting a release is a re-upload into the target index.
- **Plugin hooks have no counterpart.** Anything you drove through devpi-ldap or devpi-lockdown moves to a layer in
  front of peryx or into your own automation against its HTTP API.
- **Replica topologies collapse to one instance per site.** There is no changelog to follow; each peryx warms
  independently.
