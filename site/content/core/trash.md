+++
title = "Trash API"
description = "List soft-deleted repository records and report whether retention permits restoration."
weight = 11
+++

Deleting a hosted artifact can move its repository metadata into trash. The trash API reads those records across active
ecosystem owners. Each owner defines its delete, restore, and retention operations.

## Record schema

| Field             | Meaning                                                             |
| ----------------- | ------------------------------------------------------------------- |
| `ecosystem`       | Registered implementation identifier                                |
| `repository`      | Route that owned the deleted record                                 |
| `name`            | Ecosystem entity name                                               |
| `reference`       | Optional owner-defined reference                                    |
| `digest`          | Optional content digest                                             |
| `reason`          | Optional deletion reason                                            |
| `actor`           | Deleting identity when the caller may read it                       |
| `deleted_at_unix` | UTC Unix timestamp of deletion                                      |
| `deadline_unix`   | Time after which retention may reclaim the record                   |
| `state`           | `restorable` or `expired`                                           |
| `restorable`      | Whether retained content and the recovery window permit restoration |

The service derives `state` and `restorable` at query time. A record expires when its recovery deadline passes or the
content needed for restoration no longer exists. An expired record may remain visible for audit until a retention sweep
reclaims it.

## List records

`GET /+trash` accepts `repository`, `ecosystem`, `state`, `deadline_before`, `limit`, and `cursor`. Results use newest
deletion first. `limit` accepts 1 through 100 and defaults to 25. A response returns `next_cursor` when another page
exists.

## Inspect one record

`GET /+trash/record` identifies a record with `ecosystem`, `repository`, and `name`. The implementation may require a
`reference`, `digest`, or both to disambiguate records.

## Authorization

A local administrator can query all repositories. A repository reader or publisher must select a route covered by its
grant. A repository token reaches its own route. The actor field requires administrator access even when a
repository-scoped caller can read the rest of the record.

Authorization runs before the metadata scan. The service returns the same `404 Not Found` for an absent repository and
one outside an authenticated caller's grants. Responses use `Cache-Control: no-store` and exclude credentials, client
addresses, and authorization headers.

## Web UI extension

The shared `/admin/trash` page renders the neutral fields. Each ecosystem extension supplies names for `name` and
`reference`, plus links to its restore and deletion documentation. The page keeps filters in the request, uses cursor
pagination, and omits `actor` when the API omits it.

## Implementations

- [Ecosystem owner documentation](@/ecosystems/_index.md)
- [Retention policy](@/core/retention.md)
