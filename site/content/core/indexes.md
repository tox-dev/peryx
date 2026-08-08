+++
title = "The index model"
description = "Neutral index roles, composition rules, and lifecycle contracts shared by every ecosystem implementation."
weight = 3
+++

An index binds a route to one registered ecosystem implementation and one role. The ecosystem implementation owns the
wire protocol and artifact format. The role controls storage and composition.

## Repository schema

Each index definition contains these fields:

| Field       | Contract                                                                                 |
| ----------- | ---------------------------------------------------------------------------------------- |
| `route`     | Unique client-facing path                                                                |
| `ecosystem` | Registered implementation identifier                                                     |
| `role`      | `cached`, `hosted`, or `virtual`                                                         |
| `upstream`  | Remote source for a cached index                                                         |
| `layers`    | Ordered member routes for a virtual index                                                |
| `upload`    | Hosted layer that receives writes through a virtual index                                |
| `policy`    | Read, write, fallback, and retention rules applied before the implementation serves data |

Use `<registered-ecosystem-id>` in templates that do not target an installed implementation. A made-up identifier will
fail registration and does not make a valid neutral example.

## Roles

A **cached** index reads through one upstream. It stores metadata and content after a miss, then serves later reads from
local storage while its freshness policy permits. The upstream remains authoritative.

A **hosted** index stores publisher writes. Access grants control publication and removal. The index remains
authoritative for its records even when the content store deduplicates bytes across routes.

A **virtual** index exposes an ordered list of cached, hosted, or virtual members through one route. Its ecosystem
implementation resolves member results and maps writes to the configured hosted layer.

## Composition rules

A virtual index may contain members from one ecosystem implementation. Startup validation rejects a mixed stack, unknown
layer, route cycle, or upload target that is not hosted.

Member order is part of the repository definition. The implementation decides whether it resolves at entity, release,
reference, or file level. It must apply visibility and access policy before it returns a merged result.

{% mermaid() %}
flowchart LR
  req["resolve widgets"] --> virtual["virtual team"]
  virtual -->|"1st: hosted layer"| hosted["private candidates<br/>selected"]
  virtual -->|"2nd: cached layer"| cached["upstream candidates<br/>shadowed"]
  class hosted good
  class cached warn
{% end %}

## Shadowing

Shadowing lets a hosted candidate take precedence over a candidate from an upstream. The implementation defines the
candidate key and the scope of that precedence. A project-level implementation can hide all upstream releases for a
private name; a digest-addressed implementation can resolve a hosted reference before consulting a cache.

Put the rule at the virtual route when clients cannot enforce one source policy. Clients that add another source can
bypass the route's decision, so deployment policy must restrict alternate sources when shadowing forms a security
boundary.

## Lifecycle contract

Each implementation maps its protocol operations onto four neutral state changes:

| Change  | Result                                                                                 |
| ------- | -------------------------------------------------------------------------------------- |
| Hide    | Removes a candidate from normal resolution while retaining its record                  |
| Restore | Makes a hidden candidate eligible again                                                |
| Delete  | Removes repository metadata when the hosted repository permits deletion                |
| Reclaim | Removes unreferenced content after retention and recovery rules permit storage cleanup |

Deleting metadata does not imply immediate blob deletion. Another route may reference the same digest, and a recovery
window may retain the record. Ecosystem implementations may add reversible states such as a resolver-visible warning.

## Supported implementations

- [PyPI index roles and resolution](@/ecosystems/pypi/_index.md#the-roles-for-pypi)
- [OCI registry roles and resolution](@/ecosystems/oci/_index.md#the-roles-for-oci)
- [Shared terminology](@/core/glossary.md)
- [Registered wire standards](@/core/standards.md)
