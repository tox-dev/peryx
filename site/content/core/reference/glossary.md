+++
title = "Glossary"
description = "Shared repository, artifact, and availability terms."
weight = 6
+++

## Artifact

Immutable bytes addressed by digest. An ecosystem defines how its metadata names and groups artifacts.

## Availability mode

The `[availability].mode` value `none`, `dc`, or `ha`. Mode `none` runs one node. The distributed modes coordinate
replicas across configured datacenters.

## Authority

The right to commit mutations for an ownership group at its current epoch.

## Datacenter

A failure domain containing one or more distributed members.

## Frontier

A monotonic serial showing how much ordered state a replica or derived view has applied.

## Index

A configured artifact endpoint with one ecosystem and one role.

## Index role

The source model for an index. A role can be `cached`, `hosted`, or `virtual`.

## Member

A configured distributed node with a stable identity, datacenter, address, and role.

## Placement

Evidence that a node or datacenter holds verified bytes for a digest.

## Read-only process

A process configured with `read_only = true`. It rejects mutations without changing the availability mode.

## Reclamation

Removal of unreferenced content after reference checks, retention rules, and recovery constraints permit deletion.

## Replica

A distributed member that applies committed state and bytes and rejects client mutations.

## Shadowing

Precedence for one candidate supplied by more than one virtual-index member. The ecosystem owner defines the candidate
key. PyPI uses the distribution filename by default, so shadowing one file does not isolate every version of its
project.

## Upstream

An external source consulted by a cached index.

## Writer

A member that accepts client mutations for an ownership group.
