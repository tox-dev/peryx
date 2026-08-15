+++
title = "Ecosystems"
description = "Documentation for the ecosystem implementations linked into peryx."
sort_by = "weight"
template = "section.html"
+++

An ecosystem owner is the crate that implements one artifact protocol. It defines the stable ID, configuration,
metadata, client commands, examples, and support matrix. Each `[[index]]` selects one owner by ID. A process can
activate several owners through separate indexes; a virtual index combines indexes from one owner.

Use the owner pages for setup and protocol reference:

{{ ecosystem_owner_links() }}

The [capability contract](@/ecosystems/capabilities.md) defines registration, selection, and installation.
`peryx-plugin-registry` composes linked owners. Shared crates depend on core IDs and capability traits, not owner types.
