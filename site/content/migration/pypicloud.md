+++
title = "From pypicloud"
description = "pypicloud is archived; velodex covers its cache-on-miss fallback and hosting model in a maintained binary."
weight = 4
+++

[pypicloud](https://github.com/stevearc/pypicloud) was the closest thing to velodex in Python: private hosting
on S3/GCS/Azure/local storage with a `fallback = cache` mode that downloaded misses from PyPI, stored them, and
served them. Its repository was archived on August 27, 2023 ("Pypicloud has transitioned to maintenance mode"),
with the last release in December 2022.

## Why velodex

Feature-wise this is the most direct migration: velodex's read-through mirror is pypicloud's `fallback = cache`
made the default and [measurably fast](@/explanation/performance.md), overlays generalize its
private-over-public model with filename-level shadowing, and the project is alive. What you lose is pypicloud's
cloud-storage backends and its user/group access system; velodex stores on local disk and authenticates uploads
with a token per index.

## The renames

| pypicloud | velodex |
| --------- | ------- |
| `ppc-make-config` + `pserve config.ini` | a [TOML file](@/reference/configuration.md) + `velodex serve` |
| `pypi.fallback = cache` | the default mirror behavior |
| `pypi.fallback = redirect` / `none` | not offered; misses serve through the cache or 404 on local-only indexes |
| `storage = s3 / gcs / azure` | local `data_dir` only |
| `db = sqlalchemy / redis / dynamo` cache | embedded (redb), nothing to provision |
| access backends (config / SQL / LDAP) | one `upload_token` per local index |
| `/simple/` and `/pypi/` routes | `/{route}/simple/` |

## Pitfalls

- No object-storage backend: if your deployment depended on S3 durability, put `data_dir` on a durable volume and
  back it up (plain files; `rsync` works), or wait for a storage backend seam.
- No per-user permissions; see the [devpi page](@/migration/devpi.md) for the same caveat.
- Multiple stateless web servers sharing one cache was a pypicloud deployment shape; velodex is one process per
  data directory.
