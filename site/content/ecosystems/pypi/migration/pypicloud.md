+++
title = "From pypicloud"
description = "What pypicloud and peryx share, the cloud backends it offers, what peryx adds, and how to move off an archived project."
weight = 4
[extra]
logos = [ "logos/python.svg"]
+++

[pypicloud](https://github.com/stevearc/pypicloud) is a Pyramid application for private package hosting on local, S3,
GCS, or Azure storage. Its `fallback = cache` mode downloads misses from another index, stores them, and serves them.
The project [entered maintenance mode in 2023](https://github.com/stevearc/pypicloud/issues/325), and its repository is
archived. The latest release is 1.3.12 from December 2022 and
[declares Python 3.7 or newer](https://pypi.org/project/pypicloud/).

## Comparison against peryx

### Overlap

- **Read-through cache-on-miss.** pypicloud's `fallback = cache` is peryx's default cached-index behavior: fetch a miss,
  store it, serve it.
- **Private hosting** of your own packages, private names taking precedence over public ones.
- **Authenticated uploads.** pypicloud uses user credentials; peryx uses scoped tokens.

### pypicloud-only behavior

- **GCS and Azure storage backends.** pypicloud [supports them directly](https://pypi.org/project/pypicloud/). peryx
  supports the local filesystem and S3-compatible object storage.
- **Pluggable cache and access backends.** pypicloud
  [configures cache and access implementations](https://pypicloud.readthedocs.io/en/latest/topics/configuration.html) by
  dotted class path, with built-in SQL, Redis, or DynamoDB caches and configuration, SQL, or LDAP access backends. peryx
  embeds its redb metadata store. Artifact access uses configured or managed scoped tokens.
- **Shared metadata backend.** pypicloud's
  [cache backend](https://pypicloud.readthedocs.io/en/latest/topics/configuration.html#pypi-db) can be shared by several
  stateless web servers. Each peryx node keeps local metadata and coordinates through the selected `dc` or `ha`
  availability contract.

### peryx-only behavior

- **Active releases.** pypicloud's repository is archived, and its
  [maintenance policy](https://github.com/stevearc/pypicloud/issues/325) accepts bug fixes rather than feature work.
- **A streaming cold path.** pypicloud buffers a missed wheel fully into a `TemporaryFile`, writes it to storage and a
  cache row, and only then serves it, so the client waits for the whole download plus the disk write plus the DB commit.
  peryx [streams the bytes to the client and into the store at once](@/contributing/runtime-architecture.md).
- **Concurrency correctness.** A cold burst of clients asking pypicloud for the same wheel each download it and race to
  insert the same primary key into single-writer [SQLite](https://www.sqlite.org/); the losers surface as `HTTP 500`.
  peryx single-flights the fetch, so all waiters tail one download.
- **Content-addressed dedup and [PEP 658](https://peps.python.org/pep-0658/) metadata**, neither of which pypicloud
  offers (it stores files by `name/version/filename` and serves no `.metadata` sibling).

### Performance vs peryx

The [benchmark suite](@/core/operations/performance.md) runs both from their published packages. Cold and warm installs
through uv:

{{<bench file="install-uv" only="peryx,pypicloud" owner="pypi" />}}

The throughput workload includes the cold burst that pypicloud answers with `HTTP 500`: four clients ask for one large
wheel the instant it lands.

{{<bench file="throughput" only="peryx,pypicloud" owner="pypi" />}}

## Migration procedure

Peryx's read-through cached index matches pypicloud's `fallback = cache`. Cached state refills on first use; only hosted
uploads need to move. Map the configuration across:

| pypicloud                                | peryx                                                                     |
| ---------------------------------------- | ------------------------------------------------------------------------- |
| `ppc-make-config` + `pserve config.ini`  | a [TOML file](@/core/operations/configuration.md) + `peryx serve`         |
| `pypi.fallback = cache`                  | the default cached-index behavior                                         |
| `pypi.fallback = redirect` / `none`      | not offered; misses serve through the cache or 404 on hosted-only indexes |
| `storage = s3`                           | `[blob] backend = "s3"`                                                   |
| `storage = gcs / azure`                  | no native backend                                                         |
| `db = sqlalchemy / redis / dynamo` cache | embedded (redb), nothing to provision                                     |
| access backends (config / SQL / LDAP)    | configured or managed scoped tokens for artifact access                   |
| `/simple/` and `/pypi/` routes           | `/{route}/simple/`                                                        |

## Gotchas

- **S3 settings differ.** Configure the bucket under `[blob]`; the AWS SDK provider chain supplies credentials. The
  [object-storage guide](@/core/repositories/object-storage.md) lists durability requirements and migration limits.
- **Permissions move to grants.** Set `anonymous_read = false` and add `read`, `write`, or `delete` actions to each
  configured or managed scoped token as needed. Peryx LDAP and OIDC logins cover management and UI access, not
  artifact-client authentication.
- **Metadata stays node-local.** Shared S3 stores blobs, not redb metadata. Use `dc` or `ha` for coordinated nodes.
