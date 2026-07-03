+++
title = "Configuration"
description = "Every TOML key, flag, and default. Precedence is defaults < TOML file < flags."
weight = 1
+++

velodex reads one TOML file, passed with `--config <path>`. A few operational settings double as flags, which override
the file. Precedence is `defaults < TOML file < flags`.

## Top level

| Setting                   | Flag              | TOML key         | Default        |
| ------------------------- | ----------------- | ---------------- | -------------- |
| Bind host                 | `--host`          | `host`           | `127.0.0.1`    |
| Bind port                 | `--port`          | `port`           | `4433`         |
| Data directory            | `--data-dir`      | `data_dir`       | `velodex-data` |
| Config file               | `--config` / `-c` | (n/a)            | (none)         |
| Cache freshness (seconds) | (file only)       | `cache_ttl_secs` | `300`          |
| Indexes                   | (file only)       | `[[index]]`      | (see below)    |

`cache_ttl_secs` is a fallback: when an upstream response carries a usable `Cache-Control` lifetime (`s-maxage` or
`max-age`), that lifetime governs the page instead. The fallback applies when the header is absent,
`no-cache`/`no-store`, or zero. Artifacts never expire; they are content-addressed by sha256, so a changed upstream file
is a new entry on the page rather than a mutation.

## `[[index]]`

Each `[[index]]` table declares one index. `name` is required; exactly one of `mirror`, `local`, or `layers` selects the
kind. velodex rejects unknown keys.

| Key            | Applies to | Meaning                                                          | Default           |
| -------------- | ---------- | ---------------------------------------------------------------- | ----------------- |
| `name`         | all        | Identifier other indexes reference in `layers`                   | (required)        |
| `route`        | all        | URL prefix the index is served under                             | same as `name`    |
| `mirror`       | mirror     | Upstream simple-index URL                                        |                   |
| `username`     | mirror     | Basic-auth username for the upstream                             | (none)            |
| `password`     | mirror     | Basic-auth password for the upstream                             | (none)            |
| `token`        | mirror     | Bearer token; takes precedence over username/password            | (none)            |
| `local`        | local      | `true` marks a hosted store (implied by `upload_token`)          | `false`           |
| `upload_token` | local      | Basic-auth password uploads must present; unset disables uploads | (none)            |
| `volatile`     | local      | Allow delete and overwrite                                       | `true`            |
| `layers`       | overlay    | Ordered index names to compose; first match per filename wins    |                   |
| `upload`       | overlay    | Local layer that receives uploads                                | first local layer |

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

## `[log]`

| Key      | Values                                                                                                                                                      | Default  |
| -------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------- | -------- |
| `level`  | a [`tracing` directive](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html): `error` ... `trace`, per-module filters | `info`   |
| `format` | `pretty`, `json`                                                                                                                                            | `pretty` |
| `sink`   | `stdout`, `file`, `journald`, `syslog`                                                                                                                      | `stdout` |
| `file`   | path, required when `sink = "file"`                                                                                                                         | (none)   |

The flags `--log-level`, `--log-format`, `--log-sink`, `--log-file`, `-v`, and `-vv` override these.
