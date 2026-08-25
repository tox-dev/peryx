+++
title = "Troubleshoot pip and uv"
description = "Distinguish missing projects, offline misses, upstream failures, access denials, and upload limits."
weight = 90
+++

An installer that reports no matching distribution can have received any of these responses:

| Response                                                | Meaning                                             | Action                                        |
| ------------------------------------------------------- | --------------------------------------------------- | --------------------------------------------- |
| `404` `project ... was not found on index ...`          | Hosted members and the upstream lack the project    | Check the route and upstream                  |
| `503` `offline mode has no cached ...`                  | An offline cached index lacks the requested content | Populate it while online or disable `offline` |
| `502` `upstream is unavailable ...`                     | The upstream failed and no stored page can answer   | Retry and inspect upstream health             |
| `403` `project ... is {status}; downloads are disabled` | Project policy blocks downloads                     | Review project and revocation policy          |

An empty project page returns `200`. Use the response status and request log to distinguish it from a missing project.

## Distributed availability responses

These responses apply when `availability.mode` is `dc` or `ha`. A read-only replica refuses uploads with
`503 Service Unavailable` before reading the form. Ingress record or byte limits return `503` with `Retry-After`. A
publication admitted under a superseded home epoch returns `409 Conflict`. Retry the same upload against the current
writer. Mode `none` starts none of these distributed resources.

See [Availability behavior](@/ecosystems/pypi/reference/availability.md).

## Background jobs

Inspect `catalog_sync` and `project_sync` failures before retrying. These jobs read PyPI project and catalog metadata;
an upstream or metadata error may persist across attempts.

## Authentication

A missing, mistyped, or revoked token receives `401` with `WWW-Authenticate: Basic realm="peryx"`. A recognized token
without the project and action grant receives `403`. Check token identity after `401`; check its glob and action set
after `403`.

## Upload limits

Quota and content inspection failures include:

| Response                                                | Cause                                                          |
| ------------------------------------------------------- | -------------------------------------------------------------- |
| `403` `project size {total} would exceed limit {limit}` | Upload crosses the project byte quota                          |
| `403` `file size {size} exceeds limit {limit}`          | One distribution crosses the file limit                        |
| `413`                                                   | Distribution archive exceeds inspection size or nesting limits |
| `429` with `Retry-After`                                | Route or upstream backpressure limit                           |

Honor `Retry-After` before another request. `quota_audit = true` records a quota violation and accepts the upload, so
inspect [upload quotas](@/ecosystems/pypi/reference/policy.md#upload-quotas) when an expected denial succeeds.

See [Revoked content](@/ecosystems/pypi/revoked-content.md) for yank and digest-revocation behavior.
