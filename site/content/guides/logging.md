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

## Security Events

Index actions emit structured records on the `velodex::security` target. Use JSON output when downstream tooling needs
to filter by actor, action, target, or result.

```shell
velodex serve --log-format json --log-sink file --log-file /var/log/velodex/events.log
```

Each record sets `security_event=true` and `event=index_action`. Velodex also writes `action`, `result`, `actor`,
`index`, `local_index`, `project`, `version`, `filename`, `digest`, `count`, `changed`, `reason`,
`request_id`, and `user_agent`. Missing values use empty strings or zero values. Velodex leaves credentials, bearer
tokens, Basic auth passwords, and URL secrets out of these records.

The current action names are `token_use`, `upload`, `yank`, `unyank`, `delete`, `restore`, and `mirror_sync`. Results
are `success`, `denied`, `failure`, or `noop`.

Plain-file queries:

```shell
grep '"security_event":true' /var/log/velodex/events.log
grep '"action":"delete"' /var/log/velodex/events.log | grep '"result":"denied"'
```

JSON queries:

```shell
jq 'select(.fields.security_event == true and .fields.action == "upload")' /var/log/velodex/events.log
jq 'select(.fields.security_event == true and .fields.actor == "__token__")' /var/log/velodex/events.log
```

## Related

- Every logging flag and TOML key: [configuration](@/reference/configuration.md)
- Numbers instead of lines: [monitoring](@/guides/monitor.md)
