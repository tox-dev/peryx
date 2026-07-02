+++
title = "The index model"
description = "Mirrors, locals, and overlays; what velox borrowed from [devpi](https://devpi.net/docs/), [Artifactory](https://jfrog.com/help/r/jfrog-artifactory-documentation/virtual-repositories), and [Nexus](https://help.sonatype.com/en/repository-types.html)."
weight = 2
+++

The index servers teams run in production converged on the same shape, and velox adopts it.

## Prior art

- **devpi** builds indexes from `bases`: a stage index inherits from a mirror, uploads go to the stage, and a
  `volatile` flag guards destructive operations.
- **Artifactory** aggregates *local* and *remote* repositories into a *virtual* repository behind one URL, with a
  default deployment target for writes and local-before-remote resolution.
- **Nexus** groups *hosted* and *proxy* repositories the same way; the member order decides who wins, and the
  documentation recommends hosted first.

The shared pattern: a read-through proxy primitive, a writable hosted primitive, and an ordered composition served
at one URL where local content wins over remote.

## velox's three shapes

- A **mirror** proxies and caches one upstream, with its own credentials.
- A **local** stores uploads; `upload_token` gates writes and `volatile` gates deletion, devpi's safety flag.
- An **overlay** serves an ordered list of other indexes under one route. Resolution is first-match per filename;
  versions union. Uploads land in the overlay's designated local layer. A layer can be another overlay, which gives
  devpi-style inheritance chains.

Filename-level (rather than project-level) shadowing means you can override one broken wheel of an upstream release
while its sdist and its other wheels continue to come from the mirror.

## Removal semantics

PyPI distinguishes hiding a release from destroying it, and velox keeps both:

- **Yank** ([PEP 592](https://peps.python.org/pep-0592/)) marks a file so resolvers skip it while exact-pin installs
  still succeed. It is reversible and is the right tool for a bad release that someone may already depend on.
- **Delete** removes uploaded records outright and is only allowed on `volatile` locals. For an overlay this
  un-shadows the upstream file. The content-addressed blob stays, since another index may reference the same digest.
- **Upstream files** cannot be modified on their mirror, so yanking or deleting one through an overlay records an
  override (`yanked` or `hidden`) on the overlay's local layer. The mirror's own route is untouched, the override
  applies wherever that local layer serves, and `restore` clears a hidden marker. This is how a broken upstream
  release is pulled from your resolvers within seconds, reversibly, without forking the mirror.

## The default topology

Out of the box velox runs a pypi.org mirror plus an empty local, overlaid at `root/pypi`. One URL therefore serves
the whole public index, and the day you need to host a private package you add a token; nothing about the client
setup changes.
