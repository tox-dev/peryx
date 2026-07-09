+++
title = "Standards"
description = "How peryx relates to the interoperability standards each ecosystem defines, and where the per-ecosystem specs live."
weight = 4
+++

peryx implements the interoperability standards that let unmodified clients talk to it. Those standards are
ecosystem-specific: Python has the packaging PEPs, containers have the OCI specifications. The detailed maps live on
each ecosystem's own Standards page. This page is the frame they share.

## peryx sits on both sides

Whatever it serves, peryx is two things at once: a **server** answering your clients, and a **client** fetching from
its upstreams. So every standard has a "served" side and a "parsed" side. A [cached](@/core/indexes.md) index parses an
upstream's responses and re-serves them in the modern wire format; a [hosted](@/core/indexes.md) index validates what
you publish before it stores it. The per-ecosystem pages mark which direction each spec applies to.

## What holds across ecosystems

Two guarantees are ecosystem-neutral, and both come from the [index model](@/core/indexes.md) rather than any one
protocol:

- **Content-addressing.** Every artifact is stored and verified by the sha256 of its bytes, so a file needed by many
  projects is stored once and a client always verifies what it received against the hash the index advertised.
- **Graceful degradation.** When an upstream implements only part of its ecosystem's stack, peryx fills the gap where
  it can and degrades per file rather than per index, then re-serves the richest format its own clients can use. An
  upstream fault becomes a `502`, never a client error the caller would not retry.

## The per-ecosystem maps

- [PyPI standards](@/ecosystems/pypi/reference/standards.md): the Simple API and the packaging PEPs (503/691, 700, 592,
  658/714, 440, 427/625, core metadata, legacy JSON and upload).
- [OCI standards](@/ecosystems/oci/reference/standards.md): the distribution spec `/v2/` API, the image spec manifests
  and blobs, the referrers API, and bearer-token auth.

## In practice

- The machinery that serves these: [architecture](@/core/architecture.md)
- What each ecosystem supports: [capability matrix](@/core/capabilities.md)
