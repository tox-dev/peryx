+++
title = "Glossary"
description = "Shared architecture, index, availability, and test terms."
weight = 6
+++

## Activation

The startup step that selects owner registrations referenced by resolved indexes, registers their capabilities, and runs
their bounded installers. An inactive owner starts no work and installs no runtime state.

## Active registry

The plugin registry after selection, containing only owners referenced by resolved indexes.

## App state

The process-level state used by HTTP extraction and runtime assembly. Domain code borrows its `ServingState` or a
narrower trait.

## Artifact

Immutable bytes addressed by digest. An ecosystem owner defines how its metadata names and groups artifacts.

## Availability assembly

The `AvailabilityAssembler::assemble` step that returns routes and an unprepared runtime in
`AvailabilityInstall<Routes>`.

## Availability handle

The prepared resource owner supervised by the process. Shutdown cancels and joins resources started by the selected
availability implementation.

## Availability mode

The `[availability].mode` value `none`, `dc`, or `ha`. `none` starts no managed availability resources. `dc` and `ha`
start distributed resources from the same executable.

## Availability state install

The `AvailabilityStateInstall` projection of role, topology, blobs, analytics, authority draining, operation
observation, and other availability capabilities into serving state.

## Authority

The right to commit mutations for an ownership group at its current epoch.

## Capability

A focused, ecosystem-neutral core trait implemented by an owner or availability crate. The registry exposes only
capabilities from active owners.

## Compiled owner settings

An owner-validated index configuration stored behind a neutral type.

## Compiled registration set

All ecosystem owner registrations linked into the executable before configuration selects owners.

## Composition root

The executable that links implementations and assembles process resources. `peryx` is the composition root.

## Coverage contract

A crate, its source roots, an executed LCOV report, and the coverage-policy digest. Each contract requires exact
executable line and function coverage.

## Crate contract

The independent build, target, test, source-ownership, and coverage checks for one non-system workspace package.

## Datacenter

A failure domain containing one or more distributed members.

## Default topology collection

The configuration phase that collects default indexes from linked registrations. Any explicit `[[index]]` replaces the
collected set.

## Ecosystem owner

An implementation identified by a stable opaque string. Its crate owns protocol behavior, metadata, settings, tests,
benchmarks, and owner documentation.

## Frontier

A monotonic serial proving how much ordered state a replica or derived view has applied.

## Index

A configured artifact endpoint. It selects an ecosystem owner and an index role.

## Index role

The source model for an index: cached, hosted, or virtual.

## Integration test

A test of public collaboration between real components inside the test runner process.

## Managed availability

The `dc` or `ha` runtime assembled by `peryx-ha-distributed` through `peryx-ha` contracts.

## Member

A configured distributed node with a stable identity, datacenter, address, and role.

## Owner crate

An ecosystem implementation crate named `peryx-ecosystem-*`.

## Placement

Evidence that a node or datacenter holds verified bytes for a digest. `peryx-ha` owns the contract; storage persists the
record without applying placement policy.

## Plugin registration

A linked owner description containing its ID, default priority, settings compiler, capabilities, and installers.

## Plugin registry

The neutral service that validates registrations, resolves index owners, builds the active registration set, registers
capabilities, and runs installers.

## Prepared availability

Routes and a lifecycle handle returned by `AvailabilityRuntime::prepare` before the main listener starts.

## Read-only process

A process configured with `read_only = true`. It rejects mutations but does not select an availability mode.

## Reclamation

Removal of unreferenced content after owner reference checks, retention rules, and recovery constraints permit deletion.
Distributed coordination belongs to `peryx-ha-distributed`; storage performs the committed mutation.

## Replica

A distributed member that applies committed state and bytes and rejects client mutations.

## Serving state

The request-path services and registered capabilities shared by core, ecosystem owner, and availability code.

## System test

A test that starts the executable or an external service and observes public process behavior.

## System package

A metadata-declared test composition root for process or external-service boundaries. It cannot be a runtime dependency.

## Unit test

A test of one behavior inside its owning crate. It may use private access through an externally mounted test module and
starts neither peryx nor an external service.

## Upstream

An external source consulted by a cached index.

## Writer

A member that accepts client mutations for an ownership group.
