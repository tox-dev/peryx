+++
title = "Ecosystems"
description = "Documentation for the ecosystem implementations linked into peryx."
sort_by = "weight"
template = "section.html"
+++

An ecosystem defines one artifact format, its configuration, client commands, and support matrix. Each `[[index]]`
selects an ecosystem by ID. One process can activate several ecosystems through separate indexes; a virtual index
combines indexes from one ecosystem.

Use the owner pages for setup and protocol reference:

- [OCI](/ecosystems/oci/): Docker
- [PyPI](/ecosystems/pypi/): Python

The [activation contract](@/ecosystems/capabilities.md) defines selection, startup behavior, and unsupported features.
