+++
title = "Search API"
description = "Search the derived entity index with route, source, availability, and access filters."
weight = 8
+++

peryx maintains a derived search index for entities observed by active ecosystem owners. Search reads metadata and never
reads artifact bytes. The authoritative metadata store can rebuild the index after a schema change or restore.

## Record schema

Each implementation maps its entities to this record:

| Field           | Meaning                                               |
| --------------- | ----------------------------------------------------- |
| `display_label` | Name shown to the caller                              |
| `resource_key`  | Stable name used for matching and ordering            |
| `route`         | Repository route                                      |
| `index`         | Index name                                            |
| `ecosystem`     | Registered ecosystem identifier                       |
| `type_label`    | Implementation term for the entity                    |
| `type`          | `uploaded`, `cached`, or `override`                   |
| `available`     | Whether this instance can serve at least one artifact |
| `summary`       | Optional implementation-provided summary              |

The `type_label` field lets a mixed result page use the implementation's terminology. Clients should identify the
implementation through `ecosystem`, not by matching `type_label` text.

## Endpoints

- `GET /+search` searches readable records across configured indexes.
- `GET /{route}/+search` searches one route and ignores a conflicting `route` query value.

Both endpoints return one response schema and accept the same query fields.

## Query fields

| Field          | Values                                                      | Default | Contract                                     |
| -------------- | ----------------------------------------------------------- | ------- | -------------------------------------------- |
| `q`            | Text of 2+ characters, or `re:<expression>` for an operator | Empty   | Matches the ecosystem's search document      |
| `route`        | Configured route                                            | Any     | Restricts a global query                     |
| `type`         | `all`, `uploaded`, `cached`, `override`                     | `all`   | Restricts record source                      |
| `availability` | `all`, `local`                                              | `all`   | Restricts records by local byte availability |
| `page`         | Positive integer                                            | `1`     | Selects a result page                        |
| `page_size`    | `25`, `50`, or `100`                                        | `25`    | Sets the result count                        |

Plain text uses case-insensitive substring matching and needs at least two characters: a shorter query names no indexed
n-gram, so answering it would mean reading every indexed document. The `re:` prefix selects a case-insensitive regular
expression, which has no n-gram to seek on and therefore does read every indexed document; it is restricted to a caller
the request authenticates as an operator, and anyone else receives `403 Forbidden`. Both modes inspect an
ecosystem-provided search document, so a match does not need to appear in `display_label` or `resource_key`. A
one-character query, an invalid expression, or an unknown availability value returns `400 Bad Request`.

The ecosystem implementations build searchable text from these fields:

| Ecosystem | Category      | Fields                                                                                                                                                                                                                                                                  |
| --------- | ------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| PyPI      | Identity      | Normalized, project-detail, and core-metadata names                                                                                                                                                                                                                     |
| PyPI      | Catalog       | Versions, distribution filenames, and per-file `Requires-Python`                                                                                                                                                                                                        |
| PyPI      | Core metadata | `Requires-Python`, summary, description, author and maintainer names and email addresses, license fields and files, keywords, dependencies and extras, classifiers, import names and namespaces, project URL labels and values, home page, and description content type |
| OCI       | Repository    | Repository name and every tag                                                                                                                                                                                                                                           |

For example, a PyPI project named `acme` with the summary `Temporary upload` matches `q=temporary`. An OCI repository
named `team/app` with the tag `release-candidate` matches `q=candidate`.

peryx indexes at most 64 KiB of each search document, and both matching modes read that same window. A query and a
longer query containing it therefore return the same records; neither matches text past the limit.

## Response schema

The response echoes `query`, `route`, `type`, `availability`, `page`, and `page_size`. It adds `total` and a `results`
array of the records above. `total` counts all readable matches after policy and availability filters. Results sort by
`display_label`, `route`, then `resource_key`. Search does not rank by relevance.

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

- [Ecosystem owner documentation](@/ecosystems/_index.md)
- [Artifact source and availability](@/core/repositories/artifact-source.md)
