+++
title = "Availability"
description = "Shipped availability behavior, component boundaries, and design contracts."
sort_by = "weight"
template = "section.html"
weight = 5
+++

Start with [Availability modes](@/core/availability/high-availability.md) to configure `none` or `dc`. The binary also
accepts `ha`, where one roster address per member carries the whole peer contract: the public server answers every peer
route, the ownership Raft RPCs included. The release-status table below lists what HA still leaves undone.

## Release status

| Operation                                                                     | Status in this release | Scope or deployment gap                                                                                                                                    |
| ----------------------------------------------------------------------------- | ---------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Local operation with `mode = "none"`                                          | Shipped                | No distributed routes, listener, workers, or ownership state                                                                                               |
| Primary-to-replica metadata and blob replication                              | Shipped                | `dc` uses the public server named by each member address                                                                                                   |
| Same-datacenter placement receipts                                            | Shipped with limits    | Responses do not bind the serving node; object stores use node receipts; filesystem parent-directory sync failures are ignored                             |
| Replica heartbeat, liveness, and group readiness                              | Shipped                | These report the static DC roster; they do not elect or promote a writer                                                                                   |
| Derived-view read frontier                                                    | Shipped                | Replica page application advances the registered view frontiers before reads pass them                                                                     |
| Filesystem copy, placement reconciliation, and reclamation                    | HA components ship     | Their jobs require a nonzero ownership term, and DC supplies zero                                                                                          |
| Public topology, placement, operation, analytics, health, and readiness views | Shipped                | The generated OpenAPI document lists public distributed operations                                                                                         |
| Private listener status                                                       | Shipped                | `dc` can expose status; `ha` requires the listener for commands                                                                                            |
| Private listener commands and transfers in `dc`                               | Unavailable by design  | DC runs no ownership consensus, so mutations return `503 Service Unavailable`                                                                              |
| Voting membership, home assignment, and planned transfer                      | HA components ship     | The handlers and consensus code ship, and a group forms and commits over the public peer plane                                                             |
| Automatic failed-home selection and transfer                                  | Design                 | No runtime worker calls the failover policy or submits the transfer after liveness marks a member dead                                                     |
| PyPI ingress records and local publication                                    | Shipped                | Uploads publish at the local or assigned HA home; no transport sends an admitted intent from another datacenter to that home                               |
| PyPI write acknowledgements                                                   | Shipped with limits    | The request path checks DC receipts; the crash-recovery finalizer can record `published` from local placement without calling the acknowledgement resolver |
| OCI write acknowledgements                                                    | Not integrated         | OCI mutation paths do not call the distributed acknowledgement resolver                                                                                    |
| Visibility projection replication                                             | Design                 | The minter, envelope, projection, and snapshot types have no production caller                                                                             |
| Version negotiation and rollout preflight                                     | Design                 | The policy functions have no startup, command, or HTTP integration                                                                                         |

Private control and peer-replication routes do not appear in `peryx openapi` or `/api-docs/openapi.json`; those schemas
describe the public server. Each page below labels a procedure that depends on design-only wiring.
