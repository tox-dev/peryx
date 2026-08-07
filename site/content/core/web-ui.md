+++
title = "Web UI extension points"
description = "The shared browser shell, admin views, and contracts implemented by ecosystem-specific package browsers."
weight = 7
+++

peryx serves a server-rendered browser interface on its application port. A client bundle hydrates interactive controls,
but links, forms, tables, and status text remain usable without scripting.

## Shared pages

The core UI owns these views:

| Route                     | Data source                      | Purpose                                       |
| ------------------------- | -------------------------------- | --------------------------------------------- |
| `/`                       | discovery and summary counters   | List repositories and their roles             |
| `/admin/status`           | `GET /+status` and `GET /+stats` | Inspect runtime and storage status            |
| `/admin/topology`         | availability topology API        | Inspect node roles, health, and frontiers     |
| `/admin/placements`       | placement API                    | Inspect aggregate and per-digest availability |
| `/admin/operations`       | operation ledger API             | Inspect pending and completed writes          |
| `/admin/policy-decisions` | policy decision API              | Query repository policy outcomes              |
| `/admin/trash`            | trash API                        | Inspect recoverable repository records        |
| `/admin/analytics`        | analytics API                    | Query retained usage aggregates               |
| `/search`                 | `GET /+search`                   | Search readable entities                      |
| `/stats`                  | `GET /+stats`                    | Drill into request counters                   |

Each page applies the same field projection as its API. A restricted field renders as restricted or stays absent; the UI
does not infer health from a value the caller cannot read.

## Ecosystem extension contract

An ecosystem implementation can add browser views without changing the shared shell. It supplies:

- its display name, badge, and terminology for repositories, entities, releases, references, and files;
- repository list and detail links;
- entity and artifact detail models;
- copyable client commands for discovery responses;
- supported archive viewers and artifact actions;
- scoped counters for the dashboard.

The extension receives the resolved route and the caller's grants. It returns view data, not raw credentials or storage
paths. Core navigation uses registered routes, so an extension does not claim a global path on its own.

## Admin view rules

Public fields cover service identity and configured routes. Operator fields cover aggregate health. Administrator fields
may include upstream hosts, recent writes, peer addresses, per-digest placement, and operation identities. Repository
grants can expose repository-scoped policy, trash, quota, and analytics data without granting server-operator access.

Admin tables use bounded pages and stable cursors. Live topology uses a bounded Server-Sent Events stream. The stream
sends one projected snapshot on connect, emits after state changes, coalesces updates for slow readers, and drops a
connection that cannot drain its socket.

### Availability topology

`/admin/topology` renders one role-filtered snapshot from `GET /+availability/topology`. Public fields identify the
group, nodes, datacenters, and roles. Operators can read liveness and the committed frontier. Administrators can read
peer addresses. The page reports capture time and marks withheld values as restricted.

The live feed reads `GET /+availability/topology/stream`. It applies the same projection to each event and uses event
identifiers for reconnects. Feed state appears as text.

### Artifact placement health

`/admin/placements` reads `GET /+availability/placements`. Operators can read aggregate local, remote-only, and
unavailable counts. Administrators can page through digests and inspect datacenter placement state. Rows omit storage
paths, owners, and credentials.

### Pending operations

`/admin/operations` reads `GET /+availability/operations`. Operators can read counts for pending, published, failed, and
expired writes. Administrators can page through operation identifiers, update times, and prune deadlines. An expired
client wait does not prove that durable completion failed.

## Credential handling

Pages that accept Basic credentials keep them in request memory. They do not place credentials in URLs, browser storage,
server-rendered markup, or visible error text. Deploy the UI over HTTPS outside loopback environments.

## Accessibility

Tables use captions, column headers, and text labels for each state. Narrow layouts scroll the table container. Status
and artifact state never depend on color. Interactive disclosures and release selectors use native controls and preserve
document order.

## Client bundle

The bundle lives at `/pkg`. Without it, server-rendered pages continue to work. Hydration adds typeahead, live counters,
stored page sizes, filtering, and mutation controls.

## Supported implementations

- [PyPI browser and upload UI](@/ecosystems/pypi/_index.md#web-ui)
- [OCI repository and manifest UI](@/ecosystems/oci/_index.md#web-ui)
- [Monitoring data model](@/core/monitor.md)
- [Search API](@/core/search.md)
- [Trash API](@/core/trash.md)
