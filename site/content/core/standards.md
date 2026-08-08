+++
title = "Standards"
description = "Find the interoperability standards implemented by each ecosystem driver."
weight = 4
+++

Each ecosystem driver implements the standards used by its clients and upstream repositories. A cached repository parses
upstream responses and serves client responses. A hosted repository validates published content before storing it.

The core repository model supplies two shared properties:

- The storage layer verifies immutable content against the digest supplied by the driver.
- A driver may retain usable cached state when an upstream request fails.

Protocol negotiation, media types, response formats, digest rules, and status mappings belong to the ecosystem
references:

- [Python package standards](@/ecosystems/pypi/reference/standards.md)
- [OCI standards](@/ecosystems/oci/reference/standards.md)
- [Capability matrix](@/ecosystems/capabilities.md)

See [Architecture](@/core/architecture.md) for the boundary between core services and ecosystem drivers.
