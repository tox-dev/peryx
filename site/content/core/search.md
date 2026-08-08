+++
title = "Search API"
description = "Search the derived entity index with route, source, availability, and access filters."
weight = 8
+++

peryx maintains a derived search index for entities observed by registered ecosystem implementations. Search reads
metadata and never reads artifact bytes. The authoritative metadata store can rebuild the index after a schema change or
restore.

## Record schema

Each implementation maps its entities to this record:

| Field             | Meaning                                               |
| ----------------- | ----------------------------------------------------- |
| `display_name`    | Name shown to the caller                              |
| `normalized_name` | Stable name used for matching and ordering            |
| `route`           | Repository route                                      |
| `index`           | Index name                                            |
| `ecosystem`       | Registered ecosystem identifier                       |
| `type_label`      | Implementation term for the entity                    |
| `type`            | `uploaded`, `cached`, or `override`                   |
| `available`       | Whether this instance can serve at least one artifact |
| `summary`         | Optional implementation-provided summary              |

The `type_label` field lets a mixed result page use the implementation's terminology. Clients should identify the
implementation through `ecosystem`, not by matching `type_label` text.

## Endpoints

- `GET /+search` searches readable records across configured indexes.
- `GET /{route}/+search` searches one route and ignores a conflicting `route` query value.

Both endpoints return one response schema and accept the same query fields.

## Query fields

| Field          | Values                                  | Default | Contract                                     |
| -------------- | --------------------------------------- | ------- | -------------------------------------------- |
| `q`            | Text or `re:<expression>`               | Empty   | Matches normalized and display names         |
| `route`        | Configured route                        | Any     | Restricts a global query                     |
| `type`         | `all`, `uploaded`, `cached`, `override` | `all`   | Restricts record source                      |
| `availability` | `all`, `local`                          | `all`   | Restricts records by local byte availability |
| `page`         | Positive integer                        | `1`     | Selects a result page                        |
| `page_size`    | `25`, `50`, or `100`                    | `25`    | Sets the result count                        |

Plain text uses case-insensitive substring matching. The `re:` prefix selects the search engine's regular-expression
dialect. An invalid expression or availability value returns `400 Bad Request`.

## Response schema

The response echoes `query`, `route`, `type`, `availability`, `page`, and `page_size`. It adds `total` and a `results`
array of the records above. `total` counts all readable matches after policy and availability filters. Results sort by
display name, route, then normalized name. Search does not rank by relevance.

## Access control

The query includes the caller's read grants before counting and paging. An unreadable name contributes no row and does
not change `total`. Policy-hidden, trashed, and revoked artifacts do not make a record visible.

## Availability

An implementation computes `available` from the artifact placement projection. A record is local when at least one
eligible artifact has verified bytes on this instance. Catalog-only metadata without local bytes remains searchable
unless the caller selects `availability=local`.

## Rebuilds

Startup discards an incompatible derived index and rebuilds it from metadata. Repository mutations mark affected search
records stale. The next query refreshes them before returning a page. Each implementation documents its manual reindex
command with its client workflows.

## Implementations

- [PyPI search behavior](@/ecosystems/pypi/reference/endpoints.md#search)
- [OCI search behavior](@/ecosystems/oci/reference/endpoints.md#search)
- [Artifact source and availability](@/core/artifact-source.md)
