+++
title = "Derived-view frontiers"
description = "How a replica gates reads on the search index and other views it derives from replicated metadata."
weight = 8
+++

A replica copies authoritative metadata from the writer and rebuilds the views it derives from that metadata: the search
index, the rendered-page cache, the protocol responses a client reads. Applying the metadata and rebuilding those views
are separate steps, so for a moment the store holds a record no view reflects yet. Serving that record through a stale
view would answer a search or a project page with new metadata paired to an old view — mixed state a reader cannot tell
from a consistent one.

A replica avoids that by exposing metadata only up to its *readable frontier*: the lowest metadata serial every required
view has applied. This page describes the frontier, the views it waits on, and what a restart guarantees. It refines the
read side of the [availability contracts](@/core/availability-contracts.md) in every mode.

## Per-view frontiers

Every derived view records how far it has caught up as a *frontier*: the authoritative serial the view provably
reflects. A view reads that serial before the metadata it derives from, so the frontier is a lower bound the view has
already reached, never an optimistic target. Frontiers are durable and monotonic: a view's frontier never moves
backward, so a replayed or reordered catch-up cannot un-apply proven work, and a restart reads the last frontier a
rebuild actually reached rather than one a crash left unfinished.

The search index is the first required view every deployment runs. It persists its frontier when a refresh or a rebuild
publishes, so the frontier tracks the serial the served index reflects. As a replica applies each page, the PyPI driver
rebuilds the search documents for exactly the projects the page changed — scoped by the `(index, project)` a changed
project marker, upload record, or override names — and advances the search frontier only once every affected project is
current. The rebuild is bounded: it re-derives one project at a time from that project's stored records rather than
walking the whole index, so a large replica never loads every project to reflect one change.

## The readable frontier

The readable frontier is the minimum of the authoritative serial and every required view's frontier. When every required
view has caught up to the authority, the readable frontier equals it and the replica serves everything it has applied.
When a view lags, the readable frontier holds at that view's frontier and names the view as the one to catch up next, so
an operator sees which view pins the read side rather than an unexplained lag.

A view that fails to rebuild keeps its prior frontier instead of advancing, so a failed required view holds the readable
frontier at the last consistent serial and reports itself as the blocker. Readability resumes only once the view
rebuilds and advances its frontier.

When a replica applies a page, it rebuilds the affected project views *before* it advances the search frontier over the
page's serial, so a record becomes readable only once the view that derives from it reflects it — never in between. If a
project's rebuild fails, the driver reports the search view as blocked and the frontier holds at its prior serial; the
lazy full refresh a later search runs re-derives every project and lifts the hold once the failing input is readable
again. Rebuilding one project on one index deletes only that project's document and re-adds the fresh one, so re-running
the same apply after a crash reaches the same index. The replica exports the readable serial as
`peryx_ha_distributed_readable_serial`, so a scrape shows how far derived views trail the metadata the replica has
committed.

## Restart behavior

Because frontiers are durable, a restart never exposes metadata a view had not yet applied. If a crash lands after the
metadata commits but before a view rebuilds to it, the view's durable frontier stays below the metadata serial, so the
readable frontier holds there until the view catches up. A reader never pairs metadata from one point with a derived
view from an earlier one, across a clean shutdown or a crash.
