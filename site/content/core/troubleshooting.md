+++
title = "Troubleshooting"
description = "Use startup errors, status probes, request logs, and inspection commands to isolate failures."
weight = 14
+++

Start with the surface that reported the failure. Startup errors identify invalid configuration. Status probes report
process and dependency health. Request logs connect an HTTP response to its route and internal error.

Client response formats and retry rules belong to the ecosystem guides:

- [Troubleshoot pip and uv](@/ecosystems/pypi/guides/troubleshooting.md)
- [Troubleshoot Docker and Podman](@/ecosystems/oci/guides/troubleshooting.md)

## Startup failures

peryx validates configuration before binding its socket. Configuration precedence is
`defaults < TOML file < environment < flags`; see [Configuration](@/core/configuration.md). Check `PERYX_*` variables
and command flags when the loaded value differs from the file.

| Message                                                                     | Cause                                    | Fix                                                                     |
| --------------------------------------------------------------------------- | ---------------------------------------- | ----------------------------------------------------------------------- |
| `log sink 'file' requires a log file path (--log-file or log.file)`         | `sink = "file"` has no path              | Set `--log-file` or `log.file`, or choose another sink                  |
| `the journald log sink is only available on Linux`                          | `sink = "journald"` runs off Linux       | Use `stdout`, `file`, or `syslog`                                       |
| `` `[tls]` needs both `cert` and `key` ``                                   | The table sets one value                 | Provide both, or use `[acme]`; see [Serve HTTPS](@/core/serve-https.md) |
| `` `[acme]` needs at least one domain ``                                    | `[acme]` has no `domains`                | Add the certificate domains                                             |
| `` index needs one of `cached`, `hosted`, or `layers` ``                    | An `[[index]]` has no role               | Assign one role                                                         |
| `` `cached` and `[[index.upstream]]` are mutually exclusive ``              | One index uses both upstream forms       | Keep one form                                                           |
| `secret file {path} holds no secret`                                        | A `_file` source points at an empty file | Write the secret or remove the source                                   |
| `credential environment variable {var} is unset, empty, or not valid UTF-8` | A `_env` source has no usable value      | Export a valid value before startup                                     |

A missing `_file` or `_env` credential stops startup. `peryx index list` loads and validates the same repository
topology as `peryx serve` without opening a listener.

## Store lock and writer claim failures

One process can hold a `data_dir` lock. Stop the process that owns the directory or give the new process another data
directory.

A writer also records its `writer_identity` in the metadata store. Another identity receives
`metadata store is claimed by writer {active}; refusing {requested}`. Promote a replacement with `peryx writer promote`
during the failover procedure in [High availability](@/core/high-availability.md).

## Replica and readiness failures

A replica serves reads and refuses mutations. A writer can also refuse a mutation when it cannot meet the configured
durability contract. Protocol status codes and bodies appear in the ecosystem availability references:

- [Python package availability](@/ecosystems/pypi/reference/availability.md)
- [OCI availability](@/ecosystems/oci/reference/availability.md)

A `503` from `/+ready` means the node has not met its readiness contract. Use `/+ready?writes=true` when a load balancer
must select a writer.

## Cached repository misses

An offline cached repository does not contact its upstream. It can serve stored content, but a cold request fails. An
online repository can also fail a cold request when its upstream is unavailable. Each ecosystem maps those conditions to
its own response format; use the client guides linked above.

## Authentication and authorization

The shared access service distinguishes an invalid credential from a recognized principal without a sufficient grant.
Ecosystem protocols present that distinction through different challenges and error bodies. Check the client guide
before changing a grant.

Management routes can hide their existence from authenticated callers without the required role. A denied caller may
receive `404` instead of `403`. Create the first administrator with
[`peryx bootstrap-administrator`](@/core/bootstrap-administrator.md), then check the role grant when a known management
route returns `404`.

## Quota and rate-limit failures

Hosted writes pass through [repository quota](@/core/quotas.md), content-size, and rate-limit checks. Audit mode records
a quota violation without rejecting the write. Protocol response bodies and client retry rules appear in the ecosystem
policy and troubleshooting pages.

## Store and mirror checks

`peryx cache fsck` rehashes content and checks each content-addressed path. It prints mismatches and unreadable content
to standard output, but exits with status `0`; inspect its output.

`peryx backup verify` and `peryx mirror verify` apply equivalent checks to a backup or mirror selection. They exit with
a nonzero status when they find a problem.

## Background jobs

`peryx job list` prints recent runs. `peryx job show <id>` displays one run. Error categories `retryable_upstream` and
`retryable_timeout` can recover on a later attempt. Investigate `upstream`, `catalog_sync`, and `project_sync` before
retrying.

An administrator can send `POST /+jobs/{id}/cancel` to the process running a job. The endpoint returns `202` after it
delivers the cancellation signal, `409` when the run has finished or belongs to another process, and `404` for an
unknown or unauthorized target.

## Loaded topology

`peryx index show <index>` prints the resolved role, upstream, and offline setting without starting the server. Use it
to compare the loaded topology with the file you edited.

## Health probes

- `/+health` returns `200` after the process starts. It does not test store readiness.
- `/+ready` returns `200` or `503` for the node readiness contract.
- `/+ready?writes=true` requires a writable role.
- `/+status` reports store, upstream, and availability-role health for operators.

[Availability contracts](@/core/availability-contracts.md) defines each probe. For request detail, target one module
with a directive such as `--log-level "info,peryx_upstream=debug"`; see [Logging](@/core/logging.md).
