+++
title = "From devpi"
description = "Map devpi's indexes, commands, and knobs onto velodex; what gets faster, and what devpi still does that velodex does not."
weight = 1
[extra]
logos = [ "logos/devpi.png"]
+++

[devpi](https://devpi.net/) is the long-standing Python answer to this problem: a caching pypi.org mirror plus
user-owned indexes with inheritance, a web UI, replication, and a pluggy-based plugin ecosystem. velodex covers the same
core (read-through mirror, private uploads, shadowing composition, yank and delete, web UI) in one static binary.

## Why velodex

The [benchmark suite](@/explanation/performance.md) runs both servers from their published packages against the same
workload:

{{ bench(file="install-uv") }}

{{ bench(file="load") }}

Beyond the numbers: PEP 658 metadata is served by default (devpi 6.20 ships it as experimental, behind
`--enable-core-metadata`), Prometheus and per-file usage counters are built in rather than plugin territory, and
deployment is one process with no nginx/supervisor front (devpi's own quickstart generates those configs via
`devpi-gen-config`).

## The renames

| devpi                                        | velodex                                                                                                     |
| -------------------------------------------- | ----------------------------------------------------------------------------------------------------------- |
| `devpi-init` then `devpi-server --port 3141` | `velodex serve` (no init step)                                                                              |
| `http://host:3141/{user}/{index}/+simple/`   | `http://host:4433/{route}/simple/`                                                                          |
| `devpi index -c dev bases=root/pypi`         | an overlay index with `layers = ["dev-local", "pypi"]` in [TOML](@/reference/configuration.md)              |
| `devpi login` + `devpi upload`               | `twine upload --repository-url http://host:4433/{route}/ dist/*` (any username, `upload_token` as password) |
| `devpi remove pkg==1.0`                      | `DELETE /{route}/{project}/{version}/` ([removal guide](@/guides/remove.md))                                |
| `volatile=False`                             | `volatile = false` on the local index                                                                       |
| `mirror_whitelist`                           | not needed: local names shadow the mirror by default ([why](@/explanation/indexes.md))                      |
| `acl_upload`                                 | one `upload_token` per local index                                                                          |
| devpi-web plugin                             | built in at `/`                                                                                             |

## Pitfalls

- **No users.** devpi indexes belong to users with per-index `acl_upload`; velodex has one upload token per local index
  and open reads. If per-person write control matters, keep issuing distinct local indexes per team.
- **No replication.** devpi's primary/replica protocol has no velodex equivalent; run one velodex per site and let each
  warm itself.
- **No `push`.** Promoting a release between indexes is a re-upload in velodex.
- **No plugin hooks.** devpi-ldap, devpi-lockdown, and friends have no counterpart; velodex's extension points are its
  HTTP API and configuration.
- devpi's mirror state does not migrate and does not need to: velodex's cache refills on first use. Only your uploaded
  packages need a `twine upload` pass into the new local index.
