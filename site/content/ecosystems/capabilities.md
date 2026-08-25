+++
title = "Ecosystem activation"
description = "How configuration selects artifact formats and how startup reports unsupported behavior."
weight = 5
+++

Each `[[index]]` selects an ecosystem through the `ecosystem` key. Peryx validates the selected ID and its settings
before it opens a listener. An omitted ID selects the single default ecosystem; startup fails when no default exists or
when two ecosystems have the same priority.

## Startup behavior

1. Peryx reads and validates each index.
1. It retains the ecosystems referenced by valid indexes.
1. Each selected ecosystem adds its routes, authentication behavior, storage operations, and maintenance jobs.
1. Peryx starts serving after all selected ecosystems finish setup.

An ecosystem with no selected index adds no routes, metrics, timers, watchers, or background jobs. This keeps unused
formats out of the running service without requiring a separate executable.

## Missing support

Optional operations report that they are unsupported when the selected ecosystem does not provide them. Peryx does not
substitute a successful no-op. Configuration errors name the index and setting that failed validation; startup leaves
the listener closed.

## Supported behavior

The ecosystem documentation lists valid IDs, configuration, client commands, and failure responses:

- [OCI](/ecosystems/oci/): Docker
- [PyPI](/ecosystems/pypi/): Python

Use [configuration](@/core/operations/configuration.md) to select indexes and
[troubleshooting](@/core/operations/troubleshooting.md) when startup rejects a selection.
