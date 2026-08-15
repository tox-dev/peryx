+++
title = "Repository management API"
description = "Create, inspect, list, update, disable, and enable repository records."
weight = 15
+++

A repository record binds a stable identifier to an index route and a registered ecosystem owner. Renames and state
changes preserve the identifier. Each committed mutation creates one repository version.

## Record schema

| Field             | Mutable | Contract                                          |
| ----------------- | ------- | ------------------------------------------------- |
| `id`              | No      | Stable opaque repository identifier               |
| `route`           | No      | Unique client-facing route                        |
| `ecosystem`       | No      | Registered owner identifier                       |
| `display_name`    | Yes     | Human-readable name                               |
| `definition`      | Yes     | Schema owned and validated by the ecosystem owner |
| `state`           | Yes     | `enabled` or `disabled`                           |
| `version`         | Server  | Revision incremented by each committed mutation   |
| `created_by`      | Server  | Identity that created the record                  |
| `created_at_unix` | Server  | Creation time                                     |
| `updated_by`      | Server  | Identity that committed the current revision      |
| `updated_at_unix` | Server  | Current revision time                             |

Neutral templates use `<registered-ecosystem-id>` for `ecosystem`. Concrete definitions belong in the selected owner's
endpoint reference. The plugin registry sends each definition to that owner for validation before storage.

## Operations

| Operation | Method | Route                         | Scope                  | Precondition |
| --------- | ------ | ----------------------------- | ---------------------- | ------------ |
| List      | `GET`  | `/+repositories`              | `administration:read`  | None         |
| Create    | `POST` | `/+repositories`              | `administration:write` | None         |
| Inspect   | `GET`  | `/+repositories/{id}`         | `administration:read`  | None         |
| Update    | `PUT`  | `/+repositories/{id}`         | `administration:write` | `If-Match`   |
| Disable   | `POST` | `/+repositories/{id}/disable` | `administration:write` | `If-Match`   |
| Enable    | `POST` | `/+repositories/{id}/enable`  | `administration:write` | `If-Match`   |

List results use identifier order, an opaque cursor, and a `limit` from 1 through 100. The `state` query filters enabled
or disabled records. A null `next_cursor` marks the last page.

## Create

The create body supplies `route`, `display_name`, `ecosystem`, and `definition`. The active owner validates `definition`
before commit. Success returns the record, its `ETag`, and a `Location` header. A duplicate route returns
`409 Conflict`; unsupported media returns `415`; invalid fields or JSON return `422`.

## Conditional mutations

Inspect and create responses expose the repository version through `ETag`. Update, disable, and enable requests copy
that value into `If-Match`. A missing precondition returns `428 Precondition Required`. A malformed precondition returns
`400 Bad Request`.

If another writer commits first, the service returns `409 Conflict`, includes the current version in the body and
`ETag`, and leaves the record unchanged. The caller reads the current record, applies its change, and retries.

Update accepts `display_name` and `definition`. It cannot change the route or ecosystem owner. Disable and enable keep
the same route and identifier. Disabling a disabled repository at its current version returns the unchanged record.

## Authorization

Every operation requires local administrator authentication. A missing or wrong credential returns `401 Unauthorized`
with a Basic challenge. An authenticated caller without the required scope receives the same `404 Not Found` response as
an absent record.

## Configuration reconciliation

The process holding mutation authority reconciles configured index routes into repository records at startup. It creates
a record for a new route and reuses the identifier for a known route. An unchanged definition does not increment the
version. In `dc` and `ha` modes, replicas receive records through metadata replication and do not reconcile
configuration.

Configuration provides one-way onboarding. API changes do not rewrite the configuration file. Removing a configured
entry does not delete its stored record.

## Owner definitions

- [Ecosystem owner documentation](@/ecosystems/_index.md)
