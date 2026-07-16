+++
title = "Command line"
description = "The peryx binary's commands and flags."
weight = 4
+++

```
peryx <COMMAND>
```

## Commands

| Command          | Purpose                                                                               |
| ---------------- | ------------------------------------------------------------------------------------- |
| `serve`          | Run the server                                                                        |
| `init`           | Create the data directory and its stores, then exit                                   |
| `config-snippet` | Print `pip.conf`, `uv.toml`, or `.pypirc` for one configured index                    |
| `index`          | List and inspect the configured indexes                                               |
| `cache`          | Inspect, validate, and clean the on-disk cache                                        |
| `backup`         | Create and verify offline backups                                                     |
| `restore`        | Restore an offline backup into a data directory                                       |
| `import-dir`     | Import local wheels and sdists into a hosted index                                    |
| `policy`         | Preview index policy decisions against cached records                                 |
| `writer`         | Promote a replacement writer during manual failover                                   |
| `mirror`         | Plan, populate, and verify mirror cache contents                                      |
| `openapi`        | Print the [OpenAPI](https://www.openapis.org/) description of the HTTP API as JSON    |
| `self update`    | Replace the binary with the newest release (installer-managed builds only; see below) |

## `serve` and `init` options

| Flag                           | Meaning                                                   | Default      |
| ------------------------------ | --------------------------------------------------------- | ------------ |
| `--config <path>`              | TOML configuration file                                   | (none)       |
| `--host <addr>`                | Bind address                                              | `127.0.0.1`  |
| `--port <port>`                | Bind port                                                 | `4433`       |
| `--data-dir <path>`            | Data directory (redb store and blob cache)                | `peryx-data` |
| `--writer-identity <identity>` | Identity allowed to write the metadata store              | (none)       |
| `--offline`                    | Serve configured cached indexes from cache only           | `false`      |
| `--read-only`                  | Serve as a replica and reject client mutations with `503` | `false`      |

### Logging

| Flag                | Meaning                                                                                                                                                                    | Default  |
| ------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------- |
| `--log-level <dir>` | [`tracing` directive](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html): `error`, `warn`, `info`, `debug`, `trace`, or per-module | `info`   |
| `-v`, `-vv`         | Raise the level to debug / trace                                                                                                                                           |          |
| `--log-format <f>`  | `pretty` or `json`                                                                                                                                                         | `pretty` |
| `--log-sink <s>`    | `stdout`, `file`, `journald`, `syslog`                                                                                                                                     | `stdout` |
| `--log-file <path>` | Log file path, required with `--log-sink file`                                                                                                                             | (none)   |

Flags override the config file; see [Configuration](@/core/configuration.md) for the full precedence and the `[[index]]`
schema.

## `index`

Read the configured topology without starting the server. `list` prints one tab-separated row per index (name, route,
ecosystem, kind, uploads); `show` prints one index's details, including a virtual index's layer stack and upload target
or a cached index's upstream. `--ecosystem` filters `list` to one ecosystem (`pypi` or `oci`).

```
peryx index list [--ecosystem pypi|oci] [--config <path>] [--data-dir <path>]
peryx index show <index> [--config <path>] [--data-dir <path>]
```

## `config-snippet`

```
peryx config-snippet --base-url <url> [--config <path>] [--index <route>] <pip.conf|uv.toml|.pypirc>
```

`--base-url` is required because the CLI cannot know the public URL in front of the server. Use the origin clients see,
with any proxy path prefix and without the index route:

```shell
peryx config-snippet --base-url https://packages.example --index root/pypi pip.conf
peryx config-snippet --base-url https://packages.example --index root/pypi uv.toml
peryx config-snippet --base-url https://packages.example --index root/pypi .pypirc
```

`pip.conf` and `uv.toml` are available for read-only and writable indexes. `.pypirc` is available only when the route
has a configured hosted upload target with an `upload_token`; the output uses `<upload-token>` instead of the configured
secret.

The three output formats are PyPI client configuration, so this command targets PyPI indexes. OCI clients take no
equivalent file: `docker`, `podman`, and `crane` point at the index route on the command line and authenticate with
`docker login`; see the [OCI set-me-up](@/ecosystems/oci/_index.md).

## `mirror`

Mirror commands read the same config, `--data-dir`, and logging flags as `serve`. The index argument is a configured
index name or route. It may point at a cached index directly, or at a virtual index with one cached layer.

```shell
peryx mirror plan root/pypi --package "requests>=2,<3"
peryx mirror sync root/pypi --requirements requirements.txt
peryx mirror sync pypi --mode all --python-tag py3 --abi-tag none --platform-tag any
peryx mirror verify pypi --mode all

peryx mirror sync root/oci --image library/alpine:latest --image library/nginx:1.27
peryx mirror verify root/oci --image library/alpine:latest
```

`plan` prints the selection without writing cache records. `sync` stores it; `verify` checks cached documents and blob
digests. Output is tab-separated with one row per document, artifact, or summary count. The selection differs by
ecosystem: a PyPI index takes package selectors (the `--package`/`--requirements` and artifact-filter flags above), and
an OCI index takes `--image` references, each pulling the image's manifest and every blob it names, following a manifest
list into its per-platform manifests. Pair either with a cached index's `offline = true` to serve the mirrored set with
no upstream.

Selection starts from `[index.prefetch]` in the config and the CLI flags add or override it:

| Flag                           | Meaning                                                       |
| ------------------------------ | ------------------------------------------------------------- |
| `--mode selected`              | Use configured and CLI package selectors                      |
| `--mode all`                   | Read the upstream root Simple project list and visit projects |
| `--mode metadata-only`         | Select packages and skip artifact downloads                   |
| `--package <selector>` / `-p`  | Add a package selector such as `flask>=3,<4`                  |
| `--requirements <path>` / `-r` | Read selectors from a requirements or constraints file        |
| `--metadata-only`              | Fetch pages and metadata siblings, skip artifacts             |
| `--no-wheels`                  | Skip wheel files                                              |
| `--no-sdists`                  | Skip source distributions                                     |
| `--python-tag <tag>`           | Keep wheels with this Python tag; repeat for more tags        |
| `--abi-tag <tag>`              | Keep wheels with this ABI tag; repeat for more tags           |
| `--platform-tag <tag>`         | Keep wheels with this platform tag; repeat for more tags      |
| `--max-file-size-bytes <n>`    | Skip files larger than `n` when upstream reports a size       |

`--mode all` can walk a large upstream. Pair it with artifact filters when you only need part of the wheel matrix.

## `cache`

Cache commands read the same config and `--data-dir` flags as `serve`. Output is tab-separated with a header row, so it
can be piped to `cut`, `awk`, or a spreadsheet without scraping prose.

```shell
peryx cache list --data-dir /var/lib/peryx
peryx cache list --index pypi --project flask
peryx cache list --digest 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
peryx cache list --stale --min-age-secs 600 --min-size-bytes 1048576
peryx cache size
peryx cache fsck
peryx cache purge project --index pypi --project flask
peryx cache purge project --index pypi --project flask --yes
peryx cache purge orphaned-blobs
peryx cache purge orphaned-blobs --yes
```

`cache list` streams metadata rows and blob paths. The index/project filters apply to cached simple-index pages; the
digest filter applies to blob files. Age and size filters apply before output.

`cache size` reports cached page counts, stale page counts, page record bytes, blob counts and bytes, invalid blob-path
counts, and metadata table row counts.

`cache fsck` checks cached page records, file URL rows, [PEP 658](https://peps.python.org/pep-0658/) metadata rows,
project rows, uploads, overrides, and blob hashes. It prints `ok` when it finds no problem; otherwise it prints one row
per problem and a `problems` total.

`cache purge project` removes one project's cached simple page and project-display row. It also removes file URL and PEP
658 metadata rows for digests that no other cached page or upload record references. It does not delete blob files; run
`cache purge orphaned-blobs` after a project purge to reclaim unreferenced blobs.

Purge commands dry-run by default. Add `--yes` to delete the planned rows or blob files.

## `backup`

`backup create` reads the same config and `--data-dir` flags as `serve`.

```shell
peryx backup create --data-dir /var/lib/peryx /backups/peryx-2026-07-03
peryx backup verify /backups/peryx-2026-07-03
```

`backup create` writes a directory containing `manifest.json`, `config.toml`, `metadata/peryx.redb`, `blobs.tsv`, and
the referenced files under `blobs/sha256/...`. It copies only blob digests referenced by metadata records and streams
file copies with hash checks. It refuses an existing non-empty backup directory.

`config.toml` is an effective config snapshot. Treat the backup directory as sensitive when the config contains upload
tokens or upstream credentials.

`backup verify` rehashes the config snapshot, blob index, and each blob. It also opens the copied metadata store and
checks that every referenced digest appears in `blobs.tsv`. It prints `ok` on success; on failure it prints `problem`
rows and exits non-zero.

## `restore`

```shell
peryx restore /backups/peryx-2026-07-03 --data-dir /var/lib/peryx
peryx restore /backups/peryx-2026-07-03 --data-dir /var/lib/peryx --force
```

`restore` verifies the backup before writing. It refuses a non-empty target data directory unless `--force` is passed.
With `--force`, it replaces the target directory, then writes `peryx.redb`, `config.toml`, and the referenced blobs. It
warns when the config snapshot in the backup names a different `data_dir` than the restore target.

## `import-dir`

```shell
peryx import-dir root/pypi ./dist --data-dir /var/lib/peryx
peryx import-dir hosted ./dist --config peryx.toml
```

The index argument may be a hosted index name, a hosted route, or a virtual-index route with a hosted upload target.
`import-dir` walks the directory tree, validates `.whl` and `.tar.gz` files through the same archive and metadata checks
used for uploads, and stores accepted artifacts in the hosted index. Unsupported files are skipped; invalid distribution
files are rejected. The `.whl`/`.tar.gz` validation makes this a PyPI command; publish OCI images to a hosted index by
pushing with `docker`, `podman`, or `crane` instead (see the [OCI set-me-up](@/ecosystems/oci/_index.md)).

Output is tab-separated:

```text
status  filename  project  version  reason
```

The `status` field is `imported`, `skipped`, or `rejected`. Each row includes the file name and reason, followed by a
summary row with imported, skipped, and rejected counts.

## `policy`

Policy commands read the same config and `--data-dir` flags as `serve`.

```shell
peryx policy dry-run --data-dir /var/lib/peryx
peryx policy dry-run --index root/pypi --project flask
```

`policy dry-run` scans cached Simple pages and uploaded file records, then prints tab-separated denial rows:

```text
action  index  project  filename  version  rule  field  reason
serve   pypi   flask             project-block-list  project  project "flask" is blocked
```

It does not fetch upstreams and does not change the served index. Use it after editing `[index.policy]` and before
running `serve` with the same config.

## `writer`

Promote a replacement writer after fencing the previous writer:

```shell
peryx writer promote writer-b --config peryx.toml
```

The configured `writer_identity` is the expected current claim. `promote` atomically replaces it with the argument and
refuses a missing store, missing expected identity, or stale claim. Update `writer_identity` to the replacement before
starting the promoted node. The command does not create a data directory, copy data, or stop the previous writer; see
[High availability](@/core/high-availability.md) for the complete procedure.

## `self update`

Only binaries placed by the release installer scripts carry this command: those builds compile the `self-update` feature
and read the install receipt the installer wrote. pip-, uv-, and cargo-installed copies neither show nor need it; their
package manager owns the file ([installation](@/core/installation.md)).
