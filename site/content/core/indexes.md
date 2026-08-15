+++
title = "The index model"
description = "Neutral index roles, composition rules, and lifecycle contracts shared by every ecosystem owner."
weight = 3
+++

An index binds a route to one registered ecosystem owner and one role. The owner controls protocol behavior and artifact
metadata. The role controls source and composition rules.

## Index schema

| Field          | Contract                                                                        |
| -------------- | ------------------------------------------------------------------------------- |
| `route`        | Unique client-facing path                                                       |
| `ecosystem`    | Registered owner identifier                                                     |
| `role`         | `cached`, `hosted`, or `virtual`                                                |
| `upstream`     | Remote source for a cached index                                                |
| `layers`       | Ordered member routes for a virtual index                                       |
| `write_target` | Hosted layer that receives writes through a virtual index                       |
| `policy`       | Read, write, fallback, and retention rules applied before the owner serves data |

Use `<registered-ecosystem-id>` in neutral templates. Startup rejects an identifier that no linked owner registered.
Concrete owner settings belong in the selected owner's documentation.

## Roles

A **cached** index reads through one upstream. It stores metadata and content after a miss, then serves later reads
while its freshness policy permits. The upstream remains authoritative.

A **hosted** index stores publisher writes. Access grants control publication and removal. The index remains
authoritative for its records even when content storage deduplicates bytes across routes.

A **virtual** index exposes an ordered list of cached, hosted, or virtual members through one route. Its owner resolves
member results and maps writes to the configured hosted layer.

## Composition rules

A virtual index requires one ecosystem owner across its members. Startup rejects a mixed stack, unknown layer, route
cycle, or write target that is not hosted.

Member order is part of the index definition. The owner defines resource, group, and artifact candidates. It applies
visibility and access policy before returning a merged result.

{% mermaid() %} flowchart LR req["resolve artifact"] --> virtual["virtual index"] virtual -->|"1st: hosted layer"|
hosted\["hosted candidates<br/>selected"\] virtual -->|"2nd: cached layer"| cached\["upstream candidates<br/>shadowed"\]
class hosted good class cached warn {% end %}

## Shadowing

Shadowing gives a hosted candidate precedence over an upstream candidate. The owner defines the candidate key and the
scope of that precedence.

Put the rule at the virtual route when clients cannot enforce one source policy. A client that adds another source can
bypass the route's decision, so access policy must restrict alternate sources when shadowing forms a security boundary.

## Lifecycle contract

Each owner maps protocol operations onto neutral state changes:

| Change  | Result                                                                         |
| ------- | ------------------------------------------------------------------------------ |
| Hide    | Removes a candidate from normal resolution while retaining its record          |
| Restore | Makes a hidden candidate eligible again                                        |
| Delete  | Removes index metadata when the hosted index permits deletion                  |
| Reclaim | Removes unreferenced content after retention and recovery rules permit cleanup |

Deleting metadata does not imply blob deletion. Another index may reference the same digest, and a recovery window may
retain it. Owner crates report references through shared traits. Availability code coordinates placement and reclamation
without parsing owner metadata.

## Related documentation

- [Ecosystem owner documentation](@/ecosystems/_index.md)
- [Shared terminology](@/core/glossary.md)
- [Registered wire standards](@/core/standards.md)
