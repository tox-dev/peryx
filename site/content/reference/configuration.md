+++
title = "Configuration"
description = "Every TOML key, flag, and default. Precedence is defaults < TOML file < flags."
weight = 1
+++

velodex reads one TOML file, passed with `--config <path>`. A few operational settings double as flags, which override
the file. Precedence is `defaults < TOML file < flags`.

## Top level

| Setting | Flag | TOML key | Default |
| ------------------------- | ----------------- | ---------------- | -------------- |
| Bind host | `--host` | `host` | `127.0.0.1` |
| Bind port | `--port` | `port` | `4433` |
| Data directory | `--data-dir` | `data_dir` | `velodex-data` |
| Config file | `--config` / `-c` | (n/a) | (none) |
| Cache freshness (seconds) | (file only) | `cache_ttl_secs` | `300` |
| Indexes | (file only) | `[[index]]` | (see below) |
| Rate limits | (file only) | `[rate_limit]` | (see below) |

`cache_ttl_secs` is a fallback: when an upstream response carries a usable `Cache-Control` lifetime (`s-maxage` or
`max-age`), that lifetime governs the page instead. The fallback applies when the header is absent,
`no-cache`/`no-store`, or zero. Artifacts never expire; they are content-addressed by sha256, so a changed upstream file
is a new entry on the page rather than a mutation.

## `[[index]]`

Each `[[index]]` table declares one index. `name` is required; exactly one of `mirror`, `local`, or `layers` selects the
kind. velodex rejects unknown keys.

| Key | Applies to | Meaning | Default |
| ---------------------- | ---------- | ----------------------------------------------------------------- | ----------------- |
| `name` | all | Identifier other indexes reference in `layers` | (required) |
| `route` | all | URL prefix the index is served under | same as `name` |
| `mirror` | mirror | Upstream simple-index URL | |
| `username` | mirror | Basic-auth username for the upstream | (none) |
| `password` | mirror | Basic-auth password for the upstream | (none) |
| `token` | mirror | Bearer token; takes precedence over username/password | (none) |
| `upstream_concurrency` | mirror | Concurrent upstream fetches for this mirror; `0` disables the cap | `8` |
| `local` | local | `true` marks a hosted store (implied by `upload_token`) | `false` |
| `upload_token` | local | Basic-auth password uploads must present; unset disables uploads | (none) |
| `volatile` | local | Allow delete and overwrite | `true` |
| `layers` | overlay | Ordered index names to compose; first match per filename wins | |
| `upload` | overlay | Local layer that receives uploads | first local layer |
| `webhook` | all | Signed delivery targets for upload and index-change events | none |

A `route` is a raw URL path prefix. It must be one or more non-empty path segments separated by `/`; each segment may
contain only ASCII letters, digits, `-`, `.`, `_`, and `~`. Startup rejects routes with a leading or trailing `/`, empty
segments, percent encoding, traversal segments, control characters, spaces, and routes whose first segment is reserved
for Velodex endpoints such as `browse`, `stats`, `+stats`, `+status`, `api-docs`, `metrics`, and `pkg`.

Declaring any `[[index]]` replaces the default topology, which is:

```toml
[[index]]
name = "pypi"
mirror = "https://pypi.org/simple/"

[[index]]
name = "local"
local = true

[[index]]
name = "root/pypi"
layers = ["local", "pypi"]
upload = "local"
```

Startup rejects duplicate names, duplicate routes, invalid routes, `layers` entries that name no index, and an `upload`
target that is not a local index.

## `[rate_limit]`

Rate limits are local to one velodex process and disabled by default. When `enabled = true`, they use fixed windows and
bounded in-memory buckets; restarting the process clears the buckets. `max_clients` caps the number of client/class
buckets kept in memory. Set a class `requests` or `window_secs` to `0` to disable that class limit.

For authenticated requests, velodex hashes the `Authorization` header and uses the hash as the bucket key. It does not
store the credential value. Other requests use the peer IP address. In in-process tests and deployments without socket
peer metadata, velodex falls back to `X-Forwarded-For`, then `X-Real-IP`, then `127.0.0.1`.

| Key | Meaning | Default |
| ------------- | ------------------------------------------- | ------- |
| `enabled` | Install the HTTP request limiter | `false` |
| `max_clients` | Maximum client/class buckets kept in memory | `8192` |

Each route class is a sub-table with `requests` and `window_secs`:

| Table | Route class | Default |
| ----------------------- | ----------------------------------------------- | -------------- |
| `[rate_limit.simple]` | Simple project list and project detail pages | `600` / `60s` |
| `[rate_limit.metadata]` | PEP 658/714 `.metadata` siblings | `1200` / `60s` |
| `[rate_limit.artifact]` | Artifact downloads and archive inspection | `300` / `60s` |
| `[rate_limit.upload]` | Upload, yank, restore, and delete requests | `60` / `60s` |
| `[rate_limit.admin]` | Status, stats, metrics, and discovery endpoints | `120` / `60s` |

Example:

```toml
[rate_limit]
enabled = true
max_clients = 4096

[rate_limit.simple]
requests = 300
window_secs = 60

[[index]]
name = "pypi"
mirror = "https://pypi.org/simple/"
upstream_concurrency = 4
```

## `[[index.webhook]]`

Put webhook tables under the index that should emit them. A target on an overlay receives events for requests made
through the overlay route; the payload also names the local layer that stored the change.

```toml
[[index]]
name = "root/pypi"
layers = ["local", "pypi"]
upload = "local"

[[index.webhook]]
name = "ci"
url = "https://ci.example/hooks/velodex"
secret_env = "VELODEX_WEBHOOK_SECRET"
events = ["upload", "delete", "restore"]
```

| Key | Meaning | Default |
| ------------ | ----------------------------------------------------------------------- | ------- |
| `name` | Stable target name used in delivery logs | |
| `url` | HTTP or HTTPS endpoint that receives JSON payloads; credentials, query, and fragment are rejected | |
| `secret` | Literal HMAC signing secret | |
| `secret_env` | Environment variable that contains the HMAC signing secret | |
| `events` | Event names to send; omit or leave empty for all supported event names | all |

Use one of `secret` or `secret_env`. Supported event names are `upload`, `yank`, `unyank`, `delete`, `restore`,
`promote`, `project-status`, and `management`. Velodex emits `upload`, `yank`, `unyank`, `delete`, and `restore` from
the write endpoints in this release; the other names reserve the contract for management surfaces that use this runtime.

Velodex stores pending deliveries in the metadata database and sends them outside the request path. A failed delivery
retries up to five attempts with capped backoff of 5, 15, 45, and 135 seconds. The delivery log stores the payload,
target name, attempt count, next retry time, response status, and last error. It does not store webhook secrets.

## `[log]`

| Key | Values | Default |
| -------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------- | -------- |
| `level` | a [`tracing` directive](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html): `error` ... `trace`, per-module filters | `info` |
| `format` | `pretty`, `json` | `pretty` |
| `sink` | `stdout`, `file`, `journald`, `syslog` | `stdout` |
| `file` | path, required when `sink = "file"` | (none) |

The flags `--log-level`, `--log-format`, `--log-sink`, `--log-file`, `-v`, and `-vv` override these.
