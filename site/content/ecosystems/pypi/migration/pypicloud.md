+++
title = "From pypicloud"
description = "What pypicloud and peryx share, the cloud backends it offers, what peryx adds, and how to move off an archived project."
weight = 4
[extra]
logos = [ "logos/python.svg"]
+++

[pypicloud](https://github.com/stevearc/pypicloud) was the closest thing to peryx in Python: a
[Pyramid](https://docs.pylonsproject.org/projects/pyramid/) application offering private hosting on S3/GCS/Azure/local
storage with a `fallback = cache` mode that downloaded misses from PyPI, stored them, and served them. Its repository
was archived on August 27, 2023 ("Pypicloud has transitioned to maintenance mode"), with the last release in December
2022\. It runs today only under Python 3.10 with [SQLAlchemy](https://www.sqlalchemy.org/) pinned below 2.

## Comparison against peryx

### Overlap

- **Read-through cache-on-miss.** pypicloud's `fallback = cache` is peryx's default cached-index behavior: fetch a miss,
  store it, serve it.
- **Private hosting** of your own packages, private names taking precedence over public ones.
- **Token or user-authenticated uploads.**

### Extra: what pypicloud does that peryx does not

- **Cloud storage backends.** pypicloud stores artifacts on [S3](https://aws.amazon.com/s3/),
  [GCS](https://cloud.google.com/storage), [Azure Blob](https://azure.microsoft.com/products/storage/blobs), or local
  disk. peryx stores on local disk only.
- **Pluggable cache and access backends.** pypicloud keeps its package index in
  [SQLAlchemy](https://www.sqlalchemy.org/), [Redis](https://redis.io/), or
  [DynamoDB](https://aws.amazon.com/dynamodb/), and drives access through config, SQL, or
  [LDAP](https://en.wikipedia.org/wiki/Lightweight_Directory_Access_Protocol) user/group systems. peryx embeds its
  metadata store ([redb](https://www.redb.org/), nothing to provision) and authenticates uploads with one token per
  index.
- **Horizontal scale-out.** Several stateless pypicloud web servers can share one storage backend and cache DB. peryx is
  one process per data directory.

### Missing: what peryx adds

- **It is maintained.** pypicloud is archived and pinned to a pre-2.0 SQLAlchemy stack.
- **A streaming cold path.** pypicloud buffers a missed wheel fully into a `TemporaryFile`, writes it to storage and a
  cache row, and only then serves it, so the client waits for the whole download plus the disk write plus the DB commit.
  peryx [streams the bytes to the client and into the store at once](@/core/architecture.md).
- **Concurrency correctness.** A cold burst of clients asking pypicloud for the same wheel each download it and race to
  insert the same primary key into single-writer SQLite; the losers surface as `HTTP 500`. peryx single-flights the
  fetch, so all waiters tail one download.
- **Content-addressed dedup and [PEP 658](https://peps.python.org/pep-0658/) metadata**, neither of which pypicloud
  offers (it stores files by `name/version/filename` and serves no `.metadata` sibling).

### Performance vs peryx

The [benchmark suite](@/core/performance.md) runs both from their published packages. Cold and warm installs through uv:

{{ bench(file="install-uv", only="peryx,pypicloud") }}

The throughput workload includes the cold burst that pypicloud answers with `HTTP 500`: four clients ask for one large
wheel the instant it lands.

{{ bench(file="throughput", only="peryx,pypicloud") }}

## How to migrate

Feature-wise this is the most direct migration: peryx's read-through cached index is pypicloud's `fallback = cache` made
the default. Your cached-index state refills on first use; only hosted uploads need to move. Map the config across:

| pypicloud                                | peryx                                                                     |
| ---------------------------------------- | ------------------------------------------------------------------------- |
| `ppc-make-config` + `pserve config.ini`  | a [TOML file](@/core/configuration.md) + `peryx serve`                    |
| `pypi.fallback = cache`                  | the default cached-index behavior                                         |
| `pypi.fallback = redirect` / `none`      | not offered; misses serve through the cache or 404 on hosted-only indexes |
| `storage = s3 / gcs / azure`             | local `data_dir` only                                                     |
| `db = sqlalchemy / redis / dynamo` cache | embedded (redb), nothing to provision                                     |
| access backends (config / SQL / LDAP)    | one `upload_token` per hosted index                                       |
| `/simple/` and `/pypi/` routes           | `/{route}/simple/`                                                        |

## Gotchas

- **No object-storage backend.** If your deployment depended on S3 durability, put `data_dir` on a durable volume and
  back it up (plain files; `rsync` works), or wait for a storage backend seam.
- **No per-user permissions.** peryx authenticates uploads with a token per index and leaves reads open; see the
  [devpi page](@/ecosystems/pypi/migration/devpi.md) for the same caveat.
- **One process per data directory.** Multiple stateless web servers sharing one cache was a valid pypicloud shape; it
  is not a peryx shape. Run one peryx per site.
