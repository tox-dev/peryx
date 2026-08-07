+++
title = "Command line"
description = "The peryx binary's commands and flags."
weight = 4
+++

```
peryx <COMMAND>
```

## Commands

| Command | Purpose | | ---------------- |
------------------------------------------------------------------------------------- | | `serve` | Run the server | |
`init` | Create the data directory and its stores, then exit | | `config check` | Validate the resolved configuration
without starting the server | | `config-snippet` | Print `pip.conf`, `uv.toml`, or `.pypirc` for one configured index |
| `index` | List and inspect the configured indexes | | `job` | Inspect durable job-run history and rebuild the search
index | | `cache` | Inspect, validate, and clean the on-disk cache | | `backup` | Create and verify offline backups | |
`restore` | Restore an offline backup into a data directory | | `import-dir` | Import local wheels and sdists into a
hosted index | | `policy` | Preview index policy decisions against cached records | | `quota` | Report configured limits
and use per [repository quota](@/core/quotas.md) | | `retention` | Preview and export a repository's
[retention plan](@/core/retention.md) | | `writer` | Promote a replacement writer during manual failover | | `mirror` |
Plan, populate, and verify mirror cache contents | | `openapi` | Print the [OpenAPI](https://www.openapis.org/)
description of the HTTP API as JSON | | `self update` | Replace the binary with the newest release (installer-managed
builds only; see below) |

## `serve` and `init` options

| Flag | Meaning | Default | | ------------------------------ |
--------------------------------------------------------- | ------------ | | `--config <path>` | TOML configuration file
| (none) | | `--host <addr>` | Bind address | `127.0.0.1` | | `--port <port>` | Bind port | `4433` | |
`--data-dir <path>` | Data directory (redb store and blob cache) | `peryx-data` | | `--writer-identity <identity>` |
Identity allowed to write the metadata store | (none) | | `--offline` | Serve configured cached indexes from cache only
| `false` | | `--read-only` | Serve as a replica and reject client mutations with `503` | `false` |

### Logging

| Flag | Meaning | Default | | ------------------- |
\--------------------------------------------------------------------------------------------------------------------------------------------------------------------------
| -------- | | `--log-level <dir>` |
[`tracing` directive](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html):
`error`, `warn`, `info`, `debug`, `trace`, or per-module | `info` | | `-v`, `-vv` | Raise the level to debug / trace | |
| `--log-format <f>` | `pretty` or `json` | `pretty` | | `--log-sink <s>` | `stdout`, `file`, `journald`, `syslog` |
`stdout` | | `--log-file <path>` | Log file path, required with `--log-sink file` | (none) |

Flags override the config file; see [Configuration](@/core/configuration.md) for the full precedence and the `[[index]]`
schema.

## `config check`

Resolve the configuration from every source — file, `PERYX_*` environment variables, then these flags — and report
whether `serve` would accept it, without opening the data directory, binding a socket, or reaching an upstream. It runs
the cross-field rules (trusted publishers need a signing key, an LDAP group mapping must name a configured index, a read
replica needs a writer identity), the logging-sink check, and the full index assembly: duplicate names or routes,
virtual indexes that reference an unknown or non-hosted member, ecosystem `[policy]` and `[index.settings]` keys, secret
files that cannot be read, and webhook targets. A `0` exit status with `configuration is valid` means a restart will
start; a non-zero status prints the first problem `serve` would hit. TLS certificate material is loaded at bind time and
is not checked here.

```
peryx config check [--config <path>] [--data-dir <path>]
```

It takes the same [`serve` and `init` options](#serve-and-init-options), so a check reflects the flags and environment a
later `serve` will see.

## `index`

Read the configured topology without starting the server. `list` prints one tab-separated row per index (name, route,
ecosystem, kind, uploads); `show` prints one index's details, including a virtual index's layer stack and upload target
or a cached index's upstream. `--ecosystem` filters `list` to one ecosystem (`pypi` or `oci`).

```
peryx index list [--ecosystem pypi|oci] [--config <path>] [--data-dir <path>]
peryx index show <index> [--config <path>] [--data-dir <path>]
```

## `job`

Inspect the durable history of background jobs and run the ones an operator triggers on demand. `list` prints the most
recent runs newest-first as JSON; `show` prints one run by its `jr_…` id. `run` starts a one-shot catalog sync for a
cached repository; `reindex` rebuilds the search index; `drain` finalizes an authority's retained writes at its new home
after a failover. Every run records a durable history entry you can read back with `list` and `show`.

```
peryx job list [--data-dir <path>] [--config <path>]
peryx job show <id> [--data-dir <path>] [--config <path>]
peryx job run --repository <name> [--source <name>] [--max-projects <n>] [--concurrency <n>] [--timeout-secs <n>]
peryx job reindex [--chunk-size <n>] [--data-dir <path>] [--config <path>]
peryx job drain --authority <name> [--data-dir <path>] [--config <path>]
```

### `job reindex`

Rebuild the derived package search index from the authoritative metadata store. The search index is a cache: it normally
refreshes on its own as pages and tags are served, and a schema change discards and rebuilds it on the next start.
`reindex` is the recovery path for when that incremental refresh cannot bring the index current — after a partial
restore, say, or a bug that left the index stale. It re-derives every document and republishes the index in one
node-wide run recorded as a `search_rebuild` job.

The rebuild commits in batches of `--chunk-size` documents (default `1000`), so peak writer memory stays bounded rather
than scaling with the catalog. Each committed batch logs its progress (`indexed` of `total`) at `info`; follow the
server log, or `job show` the run, to watch a long rebuild advance.

Publication is atomic. Searches keep serving the prior complete index for the whole rebuild and switch to the new one
only once every batch has committed, so a query never sees a half-built index. If the process stops mid-rebuild, the
partial index is discarded on the next start and the incremental refresh rebuilds it — a restart never serves partial
results. A rebuild cancelled at shutdown leaves the served index untouched.

### `job drain`

Finalize the ingress write intents an authority's former home left retained, at the datacenter that just took its home.
When a home fails and the control quorum transfers an authority to a survivor, the ingress datacenters still hold the
writes the old home never finalized. `drain` reads those intents in stable key order and finalizes each into the new
home's local metadata, recording an `authority_drain` job you can read back with `list` and `show`. It is the operator
side of [authority transfer](@/core/availability-authority-transfer.md): the transfer moves the home, and the drain
settles the writes that were in flight when it moved.

The pass is bounded, ordered, and resumable. It finalizes in batches so a large backlog drains in bounded transactions,
and each finalize only advances an intent, never re-applies it, so re-running after an interruption resumes at the first
intent still pending rather than double-finalizing settled ones. Because the run names its authority, the scheduler
fences it: if the same authority transfers again while the drain runs, the run leased a now-superseded epoch and fails
with `authority_fenced` rather than finalizing under stale authority — re-run it at the current home.

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
has a hosted upload target that accepts uploads; the output uses `<upload-token>` instead of the configured secret.

The three output formats are PyPI client configuration, so this command targets PyPI indexes. OCI clients take no
equivalent file: `docker`, `podman`, and `crane` point at the index route on the command line and authenticate with
`docker login`; see the [OCI set-me-up](@/ecosystems/oci/_index.md).

## `mirror`

Mirror commands read the same config, `--data-dir`, and logging flags as `serve`. The index argument is a configured
index name or route. It may point at a cached index directly or at a virtual index with one cached layer.

```shell
peryx mirror plan <index> [ecosystem options]
peryx mirror sync <index> [ecosystem options]
peryx mirror verify <index> [ecosystem options]
```

`plan` prints the selection without writing cache records. `sync` stores it; `verify` checks cached documents and blob
digests. Output is tab-separated with one row per selected item or summary count. Core resolves the index and dispatches
to its mirror capability; the plugin combines `[index.prefetch]` with its CLI options. Pair a mirrored index with
`offline = true` to serve the stored set without an upstream.

- [Mirror PyPI packages](@/ecosystems/pypi/reference/mirroring.md)
- [Mirror OCI images](@/ecosystems/oci/reference/mirroring.md)

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
tokens or upstream credentials. On Unix, `backup create` creates the root `0700` and the config snapshot, metadata
store, and manifest `0600` regardless of umask, and `restore` writes the restored `config.toml` and `peryx.redb` `0600`.

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

## `quota`

Quota commands read the same config and `--data-dir` flags as `serve`, derive each repository's limits from its policy,
and change no metadata. They report the same status as the
[`/+quota` HTTP reads](@/core/quotas.md#reading-quota-status).

```shell
peryx quota list
peryx quota inspect --index hosted
```

`quota list` prints one tab-separated row per repository; `quota inspect` prints one repository as JSON. A `-` byte or
project limit marks an unlimited counter:

```text
repository  ecosystem  used_bytes  reserved_bytes  byte_limit  remaining_bytes  projects  project_limit  audit
hosted      pypi       3000        500             10000       6500             1         5              false
pypi        pypi       0           0               -           -                0         -              false
```

## `retention`

Retention commands read the same config and `--data-dir` flags as `serve`, load rules from a `--rules` TOML file in the
[retention configuration form](@/core/retention.md#rules), and change no metadata.

```shell
peryx retention dry-run --index root/pypi --rules retention.toml --limit 100
peryx retention export --index root/pypi --rules retention.toml > plan.jsonl
```

`retention dry-run` prints one page of tab-separated candidates, then a `summary` row and, when the page fills, a
`next-cursor` row to resume from:

```text
action  project  version  artifact                        digest      class   visibility  bytes  rule
remove  example  1.0      example-1.0-py3-none-any.whl     sha256:012  hosted  active      20480  age
summary policy_version=42  repository=7  catalog=3  policy=2
```

`retention export` streams the whole plan as JSON Lines, the identity first, matching the HTTP export. See
[Retention plans](@/core/retention.md#preview-and-export-from-the-cli) for pagination, resumable export, and the
side-effect-free contract.

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
