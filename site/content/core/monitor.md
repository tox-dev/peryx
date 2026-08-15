+++
title = "Monitoring APIs"
description = "Shared status, usage, analytics, quota, and cache-health contracts."
weight = 8
+++

peryx exposes operational data through JSON, Prometheus, and the web UI. Core defines authorization, aggregation, and
shared field names. Ecosystem guides document protocol-specific counters and client examples.

## Monitoring surfaces

| Surface             | Lifetime         | Cardinality                         | Use                                |
| ------------------- | ---------------- | ----------------------------------- | ---------------------------------- |
| `GET /+status`      | Current snapshot | Bounded by configured routes        | Runtime, storage, and cache health |
| `GET /+stats`       | Process lifetime | Repository, resource, artifact      | Usage drill-down                   |
| `GET /+analytics/*` | Retention window | Repository, resource, group, source | Historical usage                   |
| `GET /+quota*`      | Durable          | Repository                          | Committed and reserved storage     |
| `GET /metrics`      | Process lifetime | Fixed label vocabularies            | Time-series collection and alerts  |

The dashboard and admin pages render these APIs after applying the caller's field projection.

## Request counters

The stats store counts listings, artifacts, artifact bytes, metadata, writes, and implementation-defined operations. It
updates outside the response path and resets on process restart. `GET /+stats` returns a top-level summary or scopes the
result with `index` and the implementation's resource key.

Repository and resource names require `operator:read`. Implementations map their terms to the shared resource and
artifact fields.

## Daily group and source usage

A successful artifact response contributes to one daily bucket with these dimensions:

| Dimension    | Presence | Meaning                                                |
| ------------ | -------- | ------------------------------------------------------ |
| `repository` | Required | Route that served the response                         |
| `resource`   | Required | Implementation resource name                           |
| `group`      | Optional | Implementation grouping key                            |
| `source`     | Optional | Upstream used for a miss; absent for a local-store hit |
| `day`        | Required | UTC day containing completion                          |

The bucket stores a read count and delivered bytes. One completed `200` response counts once. One completed `206`
response counts once and records the range bytes. Cancelled, truncated, rejected, unauthorized, and error responses do
not count.

Retention removes whole expired days. A query that starts before the retention floor clamps its interval and sets
`window_clamped_to_retention`, which distinguishes aged-out data from an idle resource.

## Analytics API

The read-only views share `repository`, `from`, `to`, `limit`, and `cursor` fields:

| Route                       | Grouping                         |
| --------------------------- | -------------------------------- |
| `/+analytics/top-resources` | Repository and resource          |
| `/+analytics/unused`        | Repository and resource          |
| `/+analytics/groups`        | Repository, resource, and group  |
| `/+analytics/sources`       | Repository, resource, and source |
| `/+analytics/timeline`      | UTC day                          |

The response includes rows, the resolved `interval`, and `next_cursor`. Unix query times floor to UTC days. The default
window covers 30 days and a request may span at most 366 days. Rows use stable count, byte, and identity ordering.

Repository credentials can query routes they may read. An operator analytics grant can query all repositories. Source
breakdown requires operator access because upstream routing belongs to the server rather than a repository.

## Quota API

`GET /+quota` lists repository totals for an administrator. `GET /+quota/repository?repository=<route>` returns one
readable route. Responses report committed and reserved use, configured limits, and remaining headroom. They use private
cache controls.

## Check operational status

Public status contains service identity and routes. Operators can read aggregate cache-health counters. Administrators
can read upstream hosts, write state, observed resource counts, and recent writes.

Cache-health counters distinguish upstream refreshes, changed metadata, stale responses, hard upstream errors, and
digest rejections. A rejected artifact never enters the cache or usage aggregate.

The `blob_storage` object reports the selected backend, durability contract, operation support, and current
reachability. The status handler reads existing metadata and health snapshots; it does not fetch upstreams or artifacts.

## Cache inspection

The cache command surface reports stored metadata and content, validates digests, and plans repository-scoped cleanup.
Ecosystem guides provide commands with valid routes and client terms.

## Related

- [Ecosystem guides](@/ecosystems/_index.md)
- [Prometheus contract](@/core/metrics.md)
- [Web UI extension points](@/core/web-ui.md)
