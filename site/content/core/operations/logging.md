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

When distributed availability is configured, every committed write opens a
[W3C trace](https://www.w3.org/TR/trace-context/) where its acknowledgement resolves, and records what the configured
`[availability.write_ack]` policy proved. The trace and span identifiers are drawn from the operating system's entropy,
so an identifier never repeats and the sampled flag is always set: a mutation is rare next to a read, and its record
only exists to answer a question about one.

A blob write emits one `availability blob write acknowledged` event:

| Field                       | Meaning                                                                                        |
| --------------------------- | ---------------------------------------------------------------------------------------------- |
| `operation.traceparent`     | W3C trace context opened for the write                                                         |
| `operation.source`          | Node that accepted the write                                                                   |
| `operation.authority`       | Authority the write mutates                                                                    |
| `operation.epoch`           | Authority epoch the write committed under                                                      |
| `operation.serial`          | Journal serial from the write's own commit receipt, absent when the mutation journaled nothing |
| `operation.kind`            | Mutation class                                                                                 |
| `ack.policy`                | Configured durability policy                                                                   |
| `ack.outcome`               | `durable`, `pending`, or `unknown`                                                             |
| `ack.scope`                 | Durability scope proven, or `none`                                                             |
| `ack.evidence`              | `filesystem` for counted node receipts, `object-store` otherwise                               |
| `ack.nodes`                 | Datacenter members whose receipt was counted                                                   |
| `ack.required`              | Byte copies the policy required                                                                |
| `ack.remaining`             | Byte copies still outstanding                                                                  |
| `ack.bytes_acknowledged`    | Whether the byte dimension proved                                                              |
| `ack.metadata_acknowledged` | Whether the metadata dimension reached the write's serial                                      |
| `ack.bytes_expired`         | Whether the byte dimension's budget ran out                                                    |
| `ack.metadata_expired`      | Whether the metadata dimension's budget ran out                                                |
| `ack.waited_seconds`        | Time the acknowledgement spent resolving                                                       |

A blob write is datacenter-durable only once both dimensions are, so both are reported: the outcome alone does not say
which one a stalled write is waiting on. A metadata-only write, such as an OCI manifest, emits
`availability metadata write acknowledged` with the same operation fields, `ack.evidence=journal-frontier`, and a single
`ack.expired` because the journal frontier is its whole proof.

Both events carry identity and verdict only. They exclude payload bytes, metadata mutations, content references,
credentials, and private paths.

Find every write that missed its durability level, and the members that did not answer:

```shell
jq 'select(.fields.message == "availability blob write acknowledged" and .fields."ack.outcome" != "durable")' \
  /var/log/peryx/events.log
```

A replicated write also carries trace context in its operation envelope, so the producer, follower apply, and content
copy join one trace. A replay retains the trace ID and operation identity but creates a new span ID for the apply work.
A received envelope keeps the sampling its author chose, and an operation without the sampled flag emits no
`availability operation` event.

## Related

- [Logging configuration](@/core/operations/configuration.md)
- [Metrics and monitoring](@/core/operations/monitor.md)
