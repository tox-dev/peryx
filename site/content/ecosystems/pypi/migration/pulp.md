+++
title = "From Pulp"
description = "Map pulp_python repositories, distributions, storage, access, and repository versions to peryx."
weight = 9
[extra]
logos = [ "logos/pulp.png"]
+++

[pulp_python](https://pulpproject.org/pulp_python/) adds Python repositories to the Pulp content platform. Its current
feature list includes PyPI mirroring, on-demand downloads, uploads, package curation, versioned repository snapshots,
content deduplication, and local or cloud storage. A Pulp deployment separates the API, content app, task workers, and
PostgreSQL, as described in the [Pulp architecture](https://pulpproject.org/pulpcore/docs/admin/learn/architecture/).

## Peryx differences

Peryx runs the API, content routes, and background work in one executable. Its pull-through cache also composes through
virtual indexes. Pulp documents that
[chaining pull-through distributions does not work](https://pulpproject.org/pulp_python/docs/user/guides/host/).

## Configuration mapping

| Pulp (pulp_python)                           | peryx                                                                                |
| -------------------------------------------- | ------------------------------------------------------------------------------------ |
| repository + remote + distribution           | one `[[index]]` entry                                                                |
| `policy = "on_demand"` remote                | cached index                                                                         |
| pull-through cache on a distribution         | cached layer in a virtual index                                                      |
| includes/excludes curation                   | shadowing plus [yank and hide overrides](@/ecosystems/pypi/guides/remove.md)         |
| `…/pypi/{base_path}/simple/`                 | `/{route}/simple/`                                                                   |
| `pulp python content upload` or twine        | twine or `uv publish`                                                                |
| users, groups, roles, and object permissions | role grants for management; configured or managed scoped tokens for artifact clients |
| local, S3, Azure, or GCP storage             | [local or S3-compatible blob storage](@/core/repositories/object-storage.md)         |
| restorable repository version                | [backup and restore](@/core/operations/backup-restore.md) at deployment scope        |

## Pitfalls

- Pulp creates a restorable repository version for each operation. Use Peryx backup and restore only when a
  deployment-wide recovery point matches the required rollback scope; otherwise keep the versioned workflow in Pulp.
- Peryx [retention plans](@/core/repositories/retention.md) preview and export removal decisions. They are not a
  replacement for Pulp repository versions, and peryx does not apply or schedule the plan.
- Pulp supports local, S3, Azure, and GCP storage. Peryx supports local and S3-compatible storage; migrate blobs from
  Azure or GCP through the package APIs instead of copying backend keys.
