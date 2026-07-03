+++
title = "From Pulp"
description = "pulp_python covers the same ground with a platform's machinery; velodex is the same ground as one process."
weight = 9

[extra]
logos = ["logos/pulp.png"]
+++

[Pulp](https://pulpproject.org/) is a content-management platform whose
[pulp_python plugin](https://pulpproject.org/pulp_python/) is, feature for feature, velodex's closest living
relative: lazy (`on_demand`) mirroring of PyPI subsets, pull-through caching, uploads, curation with
allow/deny lists, and versioned repository snapshots. The difference is the machinery: a Pulp deployment is a
Django REST API, a separate content-serving app, one or more task workers, and PostgreSQL
([architecture](https://pulpproject.org/pulpcore/docs/admin/learn/architecture/)), and Pulp also manages RPMs,
containers, and other content types with the same model.

## Why velodex

If Python packages are the only content type you need, the platform is the overhead: four services and a
database against one binary and a data directory. velodex also serves at a plain index root instead of
`/pypi/{base_path}/simple/`, and its pull-through composes: overlays can layer mirrors of mirrors, where Pulp
documents that [chaining pull-through indices does not work](https://pulpproject.org/pulp_python/docs/user/guides/host/).

## The renames

| Pulp (pulp_python) | velodex |
| ------------------ | ------- |
| repository + remote + distribution | one `[[index]]` entry |
| `policy = "on_demand"` remote | the mirror default |
| pull-through cache on a distribution | a mirror layer in an overlay |
| includes/excludes curation | shadowing plus [yank and hide overrides](@/guides/remove.md) |
| `…/pypi/{base_path}/simple/` | `/{route}/simple/` |
| `pulp python content upload` / twine | twine / `uv publish`, unchanged |
| repository version rollback | not offered; deletes are the undo |

## Pitfalls

- Pulp's versioned snapshots ("repoint the distribution at yesterday's repository version") have no velodex
  counterpart; if you rely on them for staged promotion, keep Pulp for that flow.
- Multi-tenancy via domains, RBAC, and the task queue are platform features velodex does not replicate.
- S3/Azure/GCS artifact storage is Pulp-native; velodex stores on local disk.
