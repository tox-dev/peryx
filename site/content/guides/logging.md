+++
title = "Configure logging"
description = "Choose a level, a format, and a sink: stdout, rotating file, journald, or syslog."
weight = 9
+++

The log level comes from `--log-level {error,warn,info,debug,trace}` or the `level` key under `[log]` in the config
file. `-v` raises the level to debug and `-vv` to trace. A directive can target one module, which keeps the rest of the
output quiet:

```shell
velodex serve --log-level "info,velodex_upstream=debug"
```

velodex logs each HTTP request with its method, path, status, and latency at info, the default level, so you can watch
pip and uv take the [PEP 658](https://peps.python.org/pep-0658/) `.metadata` path without raising verbosity.

## Sinks

Output goes to one sink, selected with `--log-sink` or `[log] sink`:

- `stdout` (default): pretty for a terminal, or one JSON object per line with `--log-format json` for log aggregation.
- `file`: a daily-rotating file at `--log-file <path>`.
- `journald`: the systemd journal (Linux only).
- `syslog`: the local syslog daemon (Unix only).

In the config file:

```toml
[log]
level = "info"
format = "json"
sink = "file"
file = "/var/log/velodex/velodex.log"
```

velodex validates the combination at startup and refuses, for example, a `file` sink without a path.

## Related

- Every logging flag and TOML key: [configuration](@/reference/configuration.md)
- Numbers instead of lines: [monitoring](@/guides/monitor.md)
