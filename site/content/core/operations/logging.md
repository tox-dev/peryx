+++
title = "Configure logging"
description = "Set log levels, structured output, sinks, security events, and availability trace context."
weight = 9
+++

Set the level with `--log-level {error,warn,info,debug,trace}` or `[log] level`. `-v` selects debug and `-vv` selects
trace:

```shell
peryx serve --log-level debug
```

At the default info level, request records include the HTTP method, path, status, and latency. Ecosystem guides describe
their client request sequences.

## Sinks

`--log-sink` or `[log] sink` selects one destination:

- `stdout`: terminal text, or JSON Lines with `--log-format json`
- `file`: a rotating file at `--log-file <path>`
- `journald`: the systemd journal on Linux
- `syslog`: the local syslog daemon on Unix

```toml
[log]
level = "info"
format = "json"
sink = "file"
file = "/var/log/peryx/peryx.log"
```

Startup rejects invalid combinations, including a file sink without a path.

## Security events

Repository actions emit structured records on the `peryx::security` target. JSON output supports filtering by actor,
action, target, or result.

```shell
peryx serve --log-format json --log-sink file --log-file /var/log/peryx/events.log
```

Each repository-action record sets `security_event=true` and `event=index_action`. Shared fields include `action`,
`result`, `actor`, `presented_user`, `index`, `request_id`, `user_agent`, and `client_ip`. Ecosystem owners may add
subject identifiers. Missing string and numeric values use empty strings and zero. Records exclude credentials, bearer
tokens, Basic passwords, and URL secrets.

`actor` names the identity authentication established, and is the only field to attribute an action to. For an index
credential it is the matched token's name; for trusted publishing it is `trusted-publisher:{binding}`; for a minted
scoped token it is the subject peryx signed into it. A request that authenticated as nobody has an empty `actor`, and so
does a background action.

`presented_user` is the username the client sent, which nothing verifies. Basic credentials authenticate on the password
alone, so this names no identity even beside a full `actor`: a `twine` upload always presents `__token__`, and a client
is free to present a name that belongs to somebody else. It is kept so a failed attempt stays traceable. The field holds
at most 64 characters and drops control characters, because the client chooses its contents.

`client_ip` records the request's transport peer. When the peer matches a `rate_limit.trusted_proxies` network, the
field uses the client address from `X-Forwarded-For` or `X-Real-IP`. The rate limiter and security logger share this
trusted-proxy decision. An untrusted peer cannot override the field with a forwarding header. Background actions and
accepted requests without an attributable address use an empty string.

Server-role checks use `event=authorization`. Allowed records include `user`, `scope`, `resource_kind`, `resource`,
`result`, and `reason`. Denied records omit the resource fields and use `reason=no_grant` or
`reason=storage_unavailable`. They also omit the rejected URL and query parameters.

```shell
grep '"security_event":true' /var/log/peryx/events.log
jq 'select(.fields.security_event == true and .fields.result == "denied")' /var/log/peryx/events.log
```

## Availability trace context

When distributed availability is configured, a replicated write carries
[W3C trace context](https://www.w3.org/TR/trace-context/) in its operation envelope. The producer, follower apply, and
content copy can join one trace. A replay retains the trace ID and operation identity but creates a new span ID for the
apply work.

A sampled operation emits one `availability operation` event:

| Field                   | Meaning                                    |
| ----------------------- | ------------------------------------------ |
| `operation.source`      | Producer datacenter identity               |
| `operation.epoch`       | Authority epoch at admission               |
| `operation.serial`      | Producer operation serial                  |
| `operation.kind`        | Driver operation name                      |
| `operation.traceparent` | W3C trace context carried by the operation |

The event excludes payload bytes, metadata mutations, content references, credentials, and private paths. An operation
without the sampled trace flag emits no event.

Use the trace ID or the source and serial pair to correlate an operation across nodes:

```shell
jq 'select(.fields.message == "availability operation" and .fields."operation.serial" == 7)' \
  /var/log/peryx/events.log
```

## Related

- [Logging configuration](@/core/operations/configuration.md)
- [Metrics and monitoring](@/core/operations/monitor.md)
