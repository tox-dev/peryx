+++
title = "Command line"
description = "The velodex binary's commands and flags."
weight = 4
+++

```
velodex <COMMAND>
```

## Commands

| Command          | Purpose                                                                               |
| ---------------- | ------------------------------------------------------------------------------------- |
| `serve`          | Run the server                                                                        |
| `init`           | Create the data directory and its stores, then exit                                   |
| `config-snippet` | Print `pip.conf`, `uv.toml`, or `.pypirc` for one configured index                    |
| `cache`          | Inspect, validate, and clean the on-disk cache                                        |
| `backup`         | Create and verify offline backups                                                     |
| `restore`        | Restore an offline backup into a data directory                                       |
| `import-dir`     | Import local wheels and sdists into a hosted repository                               |
| `openapi`        | Print the OpenAPI description of the HTTP API as JSON                                 |
| `self update`    | Replace the binary with the newest release (installer-managed builds only; see below) |

## `serve` and `init` options

| Flag                | Meaning                                    | Default        |
| ------------------- | ------------------------------------------ | -------------- |
| `--config <path>`   | TOML configuration file                    | (none)         |
| `--host <addr>`     | Bind address                               | `127.0.0.1`    |
| `--port <port>`     | Bind port                                  | `4433`         |
| `--data-dir <path>` | Data directory (redb store and blob cache) | `velodex-data` |

### Logging

| Flag                | Meaning                                                                                                                                                                    | Default  |
| ------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------- |
| `--log-level <dir>` | [`tracing` directive](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html): `error`, `warn`, `info`, `debug`, `trace`, or per-module | `info`   |
| `-v`, `-vv`         | Raise the level to debug / trace                                                                                                                                           |          |
| `--log-format <f>`  | `pretty` or `json`                                                                                                                                                         | `pretty` |
| `--log-sink <s>`    | `stdout`, `file`, `journald`, `syslog`                                                                                                                                     | `stdout` |
| `--log-file <path>` | Log file path, required with `--log-sink file`                                                                                                                             | (none)   |

Flags override the config file; see [Configuration](@/reference/configuration.md) for the full precedence and the
`[[index]]` schema.

## `config-snippet`

```
velodex config-snippet --base-url <url> [--config <path>] [--index <route>] <pip.conf|uv.toml|.pypirc>
```

`--base-url` is required because the CLI cannot know the public URL in front of the server. Use the origin clients see,
with any proxy path prefix and without the index route:

```shell
velodex config-snippet --base-url https://packages.example --index root/pypi pip.conf
velodex config-snippet --base-url https://packages.example --index root/pypi uv.toml
velodex config-snippet --base-url https://packages.example --index root/pypi .pypirc
```

`pip.conf` and `uv.toml` are available for read-only and writable indexes. `.pypirc` is available only when the route
has a configured local upload target with an `upload_token`; the output uses `<upload-token>` instead of the configured
secret.

## `cache`

Cache commands read the same config and `--data-dir` flags as `serve`. Output is tab-separated with a header row, so it
can be piped to `cut`, `awk`, or a spreadsheet without scraping prose.

```shell
velodex cache list --data-dir /var/lib/velodex
velodex cache list --index pypi --project flask
velodex cache list --digest 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
velodex cache list --stale --min-age-secs 600 --min-size-bytes 1048576
velodex cache size
velodex cache fsck
velodex cache purge project --index pypi --project flask
velodex cache purge project --index pypi --project flask --yes
velodex cache purge orphaned-blobs
velodex cache purge orphaned-blobs --yes
```

`cache list` streams metadata rows and blob paths. The index/project filters apply to cached simple-index pages; the
digest filter applies to blob files. Age and size filters apply before output.

`cache size` reports cached page counts, stale page counts, page record bytes, blob counts and bytes, invalid blob-path
counts, and metadata table row counts.

`cache fsck` checks cached page records, file URL rows, PEP 658 metadata rows, project rows, uploads, overrides, and
blob hashes. It prints `ok` when it finds no problem; otherwise it prints one row per problem and a `problems` total.

`cache purge project` removes one project's cached simple page and project-display row. It also removes file URL and PEP
658 metadata rows for digests that no other cached page or upload record references. It does not delete blob files; run
`cache purge orphaned-blobs` after a project purge to reclaim unreferenced blobs.

Purge commands dry-run by default. Add `--yes` to delete the planned rows or blob files.

## `backup`

`backup create` reads the same config and `--data-dir` flags as `serve`.

```shell
velodex backup create --data-dir /var/lib/velodex /backups/velodex-2026-07-03
velodex backup verify /backups/velodex-2026-07-03
```

`backup create` writes a directory containing `manifest.json`, `config.toml`, `metadata/velodex.redb`, `blobs.tsv`, and
the referenced files under `blobs/sha256/...`. It copies only blob digests referenced by metadata records and streams
file copies with hash checks. It refuses an existing non-empty backup directory.

`config.toml` is an effective config snapshot. Treat the backup directory as sensitive when the config contains upload
tokens or upstream credentials.

`backup verify` rehashes the config snapshot, blob index, and each blob. It also opens the copied metadata store and
checks that every referenced digest appears in `blobs.tsv`. It prints `ok` on success; on failure it prints `problem`
rows and exits non-zero.

## `restore`

```shell
velodex restore /backups/velodex-2026-07-03 --data-dir /var/lib/velodex
velodex restore /backups/velodex-2026-07-03 --data-dir /var/lib/velodex --force
```

`restore` verifies the backup before writing. It refuses a non-empty target data directory unless `--force` is passed.
With `--force`, it replaces the target directory, then writes `velodex.redb`, `config.toml`, and the referenced blobs.
It warns when the config snapshot in the backup names a different `data_dir` than the restore target.

## `import-dir`

```shell
velodex import-dir root/pypi ./dist --data-dir /var/lib/velodex
velodex import-dir local ./dist --config velodex.toml
```

The repository argument may be a local index name, a local route, or an overlay route with a local upload target.
`import-dir` walks the directory tree, validates `.whl` and `.tar.gz` files through the same archive and metadata checks
used for uploads, and stores accepted artifacts in the hosted repository. Unsupported files are skipped; invalid
distribution files are rejected.

Output is tab-separated:

```text
status  filename  project  version  reason
```

The `status` field is `imported`, `skipped`, or `rejected`. Each row includes the file name and reason, followed by a
summary row with imported, skipped, and rejected counts.

## `self update`

Only binaries placed by the release installer scripts carry this command: those builds compile the `self-update` feature
and read the install receipt the installer wrote. pip-, uv-, and cargo-installed copies neither show nor need it; their
package manager owns the file ([installation](@/reference/installation.md)).
