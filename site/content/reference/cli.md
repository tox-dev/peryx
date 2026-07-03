+++
title = "Command line"
description = "The velodex binary's commands and flags."
weight = 4
+++

```
velodex <COMMAND>
```

## Commands

| Command       | Purpose                                                             |
| ------------- | ------------------------------------------------------------------- |
| `serve`       | Run the server                                                       |
| `init`        | Create the data directory and its stores, then exit                  |
| `openapi`     | Print the OpenAPI description of the HTTP API as JSON                |
| `self update` | Replace the binary with the newest release (installer-managed builds only; see below) |

## `serve` and `init` options

| Flag                | Meaning                                                    | Default       |
| ------------------- | ----------------------------------------------------------- | ------------- |
| `--config <path>`   | TOML configuration file                                     | (none)        |
| `--host <addr>`     | Bind address                                                | `127.0.0.1`   |
| `--port <port>`     | Bind port                                                   | `4433`        |
| `--data-dir <path>` | Data directory (redb store and blob cache)                  | `velodex-data`  |

### Logging

| Flag                | Meaning                                                   | Default       |
| ------------------- | ---------------------------------------------------------- | ------------- |
| `--log-level <dir>` | [`tracing` directive](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html): `error`, `warn`, `info`, `debug`, `trace`, or per-module | `info` |
| `-v`, `-vv`         | Raise the level to debug / trace                           |               |
| `--log-format <f>`  | `pretty` or `json`                                         | `pretty`      |
| `--log-sink <s>`    | `stdout`, `file`, `journald`, `syslog`                     | `stdout`      |
| `--log-file <path>` | Log file path, required with `--log-sink file`             | (none)        |

Flags override the config file; see [Configuration](@/reference/configuration.md) for the full precedence and the
`[[index]]` schema.

## `self update`

Only binaries placed by the release installer scripts carry this command: those builds compile the `self-update`
feature and read the install receipt the installer wrote. pip-, uv-, and cargo-installed copies neither show nor
need it; their package manager owns the file ([installation](@/reference/installation.md)).
