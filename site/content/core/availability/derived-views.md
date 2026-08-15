+++
title = "Derived-view frontiers"
description = "Gate replica reads until each required view reflects the applied metadata serial."
weight = 8
aliases = [ "/core/availability-derived-views/"]
+++

A replica applies authoritative metadata before rebuilding the indexes and caches derived from it. During that gap, the
metadata store can contain a record that a required view does not reflect. Serving both states in one response would
give the reader an inconsistent snapshot.

The replica exposes metadata through its *readable frontier*. This frontier is the lowest serial reached by the
authoritative store and each required derived view. Content owners register their required views through the shared
replica-view contract.

## View frontiers

Each required view records the highest authoritative serial it reflects. The view persists that serial after publishing
its rebuilt state. Its frontier moves forward and survives a restart.

A replay can rebuild the same input, but it cannot move the durable frontier backward. A failed rebuild leaves the
frontier at its last completed serial and identifies that view as the blocker.

## Readable frontier

The readable frontier is the minimum of the applied metadata serial and all required view frontiers. It equals the
metadata serial after all views catch up. A lagging or failed view holds it at the last consistent serial.

The apply path rebuilds affected views before advancing their frontiers past a metadata page. A record becomes readable
after each required view includes it. The metric `peryx_ha_distributed_readable_serial` reports the resulting serial.

## Restart behavior

A crash between metadata commit and view publication leaves the durable view frontier below the metadata serial. After
restart, the replica keeps reads behind that frontier until the rebuild completes. The same rule applies after a clean
shutdown.
