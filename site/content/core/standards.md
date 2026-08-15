+++
title = "Standards"
description = "Find the interoperability standards implemented by each ecosystem."
weight = 4
+++

Each owner crate implements its protocol and registers ecosystem-neutral core capabilities. A cached repository parses
upstream responses and serves client responses. A hosted repository validates incoming content before storing it.

The core repository model supplies two shared properties:

- The storage layer verifies immutable content against the digest supplied by the owner.
- An owner may retain usable cached state when an upstream request fails.

Protocol negotiation, media types, response formats, digest rules, and status mappings belong to the ecosystem
references:

- [Ecosystem owner documentation](@/ecosystems/_index.md)
- [Capability matrix](@/ecosystems/capabilities.md)

See [code architecture](@/contributing/architecture.md) for the boundary between core contracts and ecosystem crates.
