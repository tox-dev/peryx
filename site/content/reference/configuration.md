+++
title = "Configuration"
description = "Every TOML key, flag, and default. Precedence is defaults < TOML file < environment < flags."
weight = 1
+++

velodex reads one TOML file, passed with `--config <path>`. A few operational settings double as flags or `VELODEX_*`
environment variables, which override the file. Precedence is `defaults < TOML file < environment < flags`.

## Top level

| Setting                   | Flag              | Environment              | TOML key         | Default        |
| ------------------------- | ----------------- | ------------------------ | ---------------- | -------------- |
| Bind host                 | `--host`          | `VELODEX_HOST`           | `host`           | `127.0.0.1`    |
| Bind port                 | `--port`          | `VELODEX_PORT`           | `port`           | `4433`         |
| Data directory            | `--data-dir`      | `VELODEX_DATA_DIR`       | `data_dir`       | `velodex-data` |
| Offline mode              | `--offline`       | `VELODEX_OFFLINE`        | `offline`        | `false`        |
| Config file               | `--config` / `-c` | (n/a)                    | (n/a)            | (none)         |
| Cache freshness (seconds) | (file/env only)   | `VELODEX_CACHE_TTL_SECS` | `cache_ttl_secs` | `300`          |
| Indexes                   | (file only)       | (n/a)                    | `[[index]]`      | (see below)    |
| Rate limits               | (file only)       | (n/a)                    | `[rate_limit]`   | (see below)    |

Environment variables sit between the file and flags: a `VELODEX_*` value overrides the TOML file, and a flag overrides
the variable. Only scalar settings are environment-configurable — the `[[index]]` topology and `[rate_limit]` block stay
file-only, since neither maps cleanly to a flat variable. An empty variable is treated as unset. The `[log]` block also
reads variables (`VELODEX_LOG_LEVEL`, `VELODEX_LOG_FORMAT`, `VELODEX_LOG_SINK`, `VELODEX_LOG_FILE`); see
[`[log]`](#log).

`cache_ttl_secs` is a fallback: when an upstream response carries a usable `Cache-Control` lifetime (`s-maxage` or
`max-age`), that lifetime governs the page instead. The fallback applies when the header is absent,
`no-cache`/`no-store`, or zero. Artifacts never expire; they are content-addressed by sha256, so a changed upstream file
is a new entry on the page rather than a mutation.

`offline = true` disables upstream network access for configured cached indexes. Cached project pages, PEP 658 metadata
siblings, and artifacts serve from disk. A cold cached-index miss returns `503`; virtual-index routes still serve any
hosted layer that can answer. Use `velodex mirror sync` before enabling offline mode on a machine that must run without
network access.

## `[[index]]`

Each `[[index]]` table declares one index. `name` is required; exactly one of `cached`, `hosted`, or `layers` selects
the role. velodex rejects unknown keys.

| Key                    | Role    | Meaning                                                               | Default            |
| ---------------------- | ------- | --------------------------------------------------------------------- | ------------------ |
| `name`                 | all     | Identifier other indexes reference in `layers`                        | (required)         |
| `route`                | all     | URL prefix the index is served under                                  | same as `name`     |
| `cached`               | cached  | Upstream simple-index URL to cache                                    |                    |
| `username`             | cached  | Basic-auth username for the upstream                                  | (none)             |
| `password`             | cached  | Basic-auth password for the upstream                                  | (none)             |
| `token`                | cached  | Bearer token; takes precedence over username/password                 | (none)             |
| `upstream_concurrency` | cached  | Cap on concurrent upstream fetches; `0` is unlimited and the default  | `0`                |
| `offline`              | cached  | Serve this cached index from disk only                                | `false`            |
| `prefetch`             | cached  | Package and artifact selection for `velodex mirror`                   | (see below)        |
| `hosted`               | hosted  | `true` marks this index as a hosted store (implied by `upload_token`) | `false`            |
| `upload_token`         | hosted  | Basic-auth password uploads must present; unset disables uploads      | (none)             |
| `volatile`             | hosted  | Allow delete and overwrite                                            | `true`             |
| `layers`               | virtual | Ordered index names to compose; first match per filename wins         |                    |
| `upload`               | virtual | Hosted layer that receives uploads                                    | first hosted layer |
| `policy`               | all     | Nested index policy table                                             | empty              |
| `webhook`              | all     | Signed delivery targets for upload and index-change events            | none               |

A `route` is a raw URL path prefix. It must be one or more non-empty path segments separated by `/`; each segment may
contain only ASCII letters, digits, `-`, `.`, `_`, and `~`. Startup rejects routes with a leading or trailing `/`, empty
segments, percent encoding, traversal segments, control characters, spaces, and routes whose first segment is reserved
for Velodex endpoints such as `browse`, `stats`, `+stats`, `+status`, `api-docs`, `metrics`, and `pkg`.

Declaring any `[[index]]` replaces the default topology, which is:

```toml
[[index]]
name = "pypi"
cached = "https://pypi.org/simple/"

[[index]]
name = "hosted"
hosted = true

[[index]]
name = "root/pypi"
layers = ["hosted", "pypi"]
upload = "hosted"
```

Startup rejects duplicate names, duplicate routes, invalid routes, `layers` entries that name no index, and an `upload`
target that is not a hosted index.

### `[index.policy]`

Policy rules apply to the index that owns the table. A cached-index policy filters that cache; a hosted policy filters
direct uploads and hosted-route reads; a virtual policy filters the merged index clients use. Project names are compared
after PEP 503 normalization.

```toml
[[index]]
name = "root/pypi"
layers = ["hosted", "pypi"]
upload = "hosted"

[index.policy]
allow_projects = ["flask", "requests"]
block_projects = ["bad-package"]
allow_versions = ">=1,<3"
allow_package_types = ["wheel"]
block_package_types = ["sdist"]
allow_wheel_pythons = ["py3", "cp313"]
block_wheel_platforms = ["win_amd64"]
max_file_size_bytes = 104857600
max_project_size_bytes = 1073741824
```

| Key                      | Meaning                                                                       |
| ------------------------ | ----------------------------------------------------------------------------- |
| `allow_projects`         | Only these normalized projects may be served, mirrored, or uploaded           |
| `block_projects`         | These normalized projects are denied                                          |
| `allow_versions`         | PEP 440 specifier set accepted for parsed distribution filenames              |
| `allow_package_types`    | Accepted parsed file types: `wheel`, `sdist`                                  |
| `block_package_types`    | Denied parsed file types: `wheel`, `sdist`                                    |
| `allow_wheel_pythons`    | Accepted wheel Python tags, matched against each dot-compressed tag segment   |
| `block_wheel_pythons`    | Denied wheel Python tags                                                      |
| `allow_wheel_platforms`  | Accepted wheel platform tags, matched against each dot-compressed tag segment |
| `block_wheel_platforms`  | Denied wheel platform tags                                                    |
| `max_file_size_bytes`    | Maximum file size from the Simple API `size` field or from an uploaded file   |
| `max_project_size_bytes` | Maximum sum of retained file sizes for one project detail page                |

File and project size rules require declared sizes. A file without `size` is denied by `max_file_size_bytes`; a project
page with any retained file lacking `size` is denied by `max_project_size_bytes`. Active policies use the buffered
Simple-page path so file lists and PEP 691 `versions` are filtered together before velodex serves bytes.

### `[index.prefetch]`

Cached indexes can declare the default selection for `velodex mirror plan`, `velodex mirror sync`, and
`velodex mirror verify`. CLI flags add package selectors and override booleans or `mode` for one run.

```toml
[[index]]
name = "pypi"
cached = "https://pypi.org/simple/"

[index.prefetch]
mode = "selected"
packages = ["requests>=2,<3"]
requirements = ["requirements.txt"]
include_wheels = true
include_sdists = true
python_tags = ["py3", "cp312"]
abi_tags = ["none", "abi3"]
platform_tags = ["any", "manylinux_2_28_x86_64"]
max_file_size_bytes = 524288000
metadata_only = false
```

| Key                   | Values                               | Default    |
| --------------------- | ------------------------------------ | ---------- |
| `mode`                | `selected`, `all`, `metadata-only`   | `selected` |
| `packages`            | package selectors such as `flask>=3` | `[]`       |
| `requirements`        | requirements or constraints files    | `[]`       |
| `include_wheels`      | boolean                              | `true`     |
| `include_sdists`      | boolean                              | `true`     |
| `python_tags`         | wheel Python tags                    | `[]`       |
| `abi_tags`            | wheel ABI tags                       | `[]`       |
| `platform_tags`       | wheel platform tags                  | `[]`       |
| `max_file_size_bytes` | positive integer                     | (none)     |
| `metadata_only`       | boolean                              | `false`    |

`mode = "all"` reads the upstream root Simple project list and then visits matching projects. Artifact filters apply
after a project page is fetched. `mode = "metadata-only"` implies `metadata_only = true`.

## `[rate_limit]`

Rate limits are local to one velodex process and disabled by default. When `enabled = true`, they use fixed windows and
bounded in-memory buckets; restarting the process clears the buckets. `max_clients` caps the number of client/class
buckets kept in memory. Set a class `requests` or `window_secs` to `0` to disable that class limit.

For authenticated requests, velodex hashes the `Authorization` header and uses the hash as the bucket key. It does not
store the credential value. Other requests use the peer IP address. In in-process tests and deployments without socket
peer metadata, velodex falls back to `X-Forwarded-For`, then `X-Real-IP`, then `127.0.0.1`.

| Key           | Meaning                                     | Default |
| ------------- | ------------------------------------------- | ------- |
| `enabled`     | Install the HTTP request limiter            | `false` |
| `max_clients` | Maximum client/class buckets kept in memory | `8192`  |

Each route class is a sub-table with `requests` and `window_secs`:

| Table                   | Route class                                     | Default        |
| ----------------------- | ----------------------------------------------- | -------------- |
| `[rate_limit.listing]`  | Project listing and detail pages                | `600` / `60s`  |
| `[rate_limit.metadata]` | PEP 658/714 `.metadata` siblings                | `1200` / `60s` |
| `[rate_limit.artifact]` | Artifact downloads and archive inspection       | `300` / `60s`  |
| `[rate_limit.upload]`   | Upload, yank, restore, and delete requests      | `60` / `60s`   |
| `[rate_limit.admin]`    | Status, stats, metrics, and discovery endpoints | `120` / `60s`  |

Example:

```toml
[rate_limit]
enabled = true
max_clients = 4096

[rate_limit.listing]
requests = 300
window_secs = 60

[[index]]
name = "pypi"
cached = "https://pypi.org/simple/"
upstream_concurrency = 4
```

## `[[index.webhook]]`

Put webhook tables under the index that should emit them. A target on a virtual index receives events for requests made
through the virtual-index route; the payload also names the hosted layer that stored the change.

```toml
[[index]]
name = "root/pypi"
layers = ["hosted", "pypi"]
upload = "hosted"

[[index.webhook]]
name = "ci"
url = "https://ci.example/hooks/velodex"
secret_env = "VELODEX_WEBHOOK_SECRET"
events = ["upload", "delete", "restore"]
```

| Key          | Meaning                                                                                           | Default |
| ------------ | ------------------------------------------------------------------------------------------------- | ------- |
| `name`       | Stable target name used in delivery logs                                                          |         |
| `url`        | HTTP or HTTPS endpoint that receives JSON payloads; credentials, query, and fragment are rejected |         |
| `secret`     | Literal HMAC signing secret                                                                       |         |
| `secret_env` | Environment variable that contains the HMAC signing secret                                        |         |
| `events`     | Event names to send; omit or leave empty for all supported event names                            | all     |

Use one of `secret` or `secret_env`. Supported event names are `upload`, `yank`, `unyank`, `delete`, `restore`,
`promote`, `project-status`, and `management`. Velodex emits `upload`, `yank`, `unyank`, `delete`, and `restore` from
the write endpoints in this release; the other names reserve the contract for management surfaces that use this runtime.

Velodex stores pending deliveries in the metadata database and sends them outside the request path. A failed delivery
retries up to five attempts with capped backoff of 5, 15, 45, and 135 seconds. The delivery log stores the payload,
target name, attempt count, next retry time, response status, and last error. It does not store webhook secrets.

## `[log]`

| Key      | Values                                                                                                                                                      | Default  |
| -------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------- | -------- |
| `level`  | a [`tracing` directive](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html): `error` ... `trace`, per-module filters | `info`   |
| `format` | `pretty`, `json`                                                                                                                                            | `pretty` |
| `sink`   | `stdout`, `file`, `journald`, `syslog`                                                                                                                      | `stdout` |
| `file`   | path, required when `sink = "file"`                                                                                                                         | (none)   |

The flags `--log-level`, `--log-format`, `--log-sink`, `--log-file`, `-v`, and `-vv` override these, as do the
`VELODEX_LOG_LEVEL`, `VELODEX_LOG_FORMAT`, `VELODEX_LOG_SINK`, and `VELODEX_LOG_FILE` variables (below the flags in
precedence).
