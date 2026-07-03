+++
title = "The index model"
description = "Mirrors, locals, and overlays: how composition works, why shadowing is the dependency-confusion fix, and what removal means."
weight = 3
+++

An index server earns its keep the day you have a package that must not come from the public internet. This page
explains velodex's answer: three index shapes and one composition rule, and why that rule is a security control
before it is a convenience.

## Prior art

The index servers teams run in production converged on the same shape, and velodex adopts it:

- **Artifactory** aggregates *local* and *remote* repositories into a *virtual* repository behind one URL, with a
  default deployment target for writes and local-before-remote resolution.
- **Nexus** groups *hosted* and *proxy* repositories the same way; the member order decides who wins, and the
  documentation recommends hosted first.

The shared pattern: a read-through proxy primitive, a writable hosted primitive, and an ordered composition served
at one URL where local content wins over remote.

## velodex's three shapes

- A **mirror** proxies and caches one upstream, with its own credentials.
- A **local** stores uploads; `upload_token` gates writes and `volatile` gates deletion.
- An **overlay** serves an ordered list of other indexes under one route. Resolution is first-match per filename;
  versions union. Uploads land in the overlay's designated local layer. A layer can be another overlay, which gives
  inheritance chains.

{% mermaid() %}
flowchart LR
  req["GET simple/utils/"] --> overlay["overlay root/pypi"]
  overlay -->|"1st: local layer"| local["your uploads<br/>utils-2.0 ✓"]
  overlay -->|"2nd: mirror layer"| mirror["pypi.org mirror<br/>utils-9.9 ✗ shadowed"]
  classDef good fill:#009E73,stroke:#009E73,color:#ffffff
  classDef warn fill:#D55E00,stroke:#D55E00,color:#ffffff
  class local good
  class mirror warn
{% end %}

Filename-level (rather than project-level) shadowing means you can override one broken wheel of an upstream release
while its sdist and its other wheels continue to come from the mirror.

## Why shadowing is a security control

The usual way to mix private and public packages is client-side: a private index in `--extra-index-url`, pypi.org
as the default. pip treats both indexes as equals and installs whichever offers the higher version. Anyone who
registers your internal package's name on pypi.org with version `99.0` now wins the race. This is
[dependency confusion](https://medium.com/@alex.birsan/dependency-confusion-4a5d60fec610), the technique that
compromised [PyTorch's nightly channel](https://pytorch.org/blog/compromised-nightly-dependency/) and, in its
original disclosure, three dozen major companies. Client-side mitigations exist (uv's `explicit` index pinning,
for one) but must be repeated in every project, for every tool, forever.

An overlay moves the decision server-side. The client has one `index-url` and no fallback; the overlay's local
layer is consulted first; a name that exists locally never falls through to the mirror. The guarantee holds for
pip, uv, poetry, and whatever comes next, because it lives where the indexes meet rather than in each client's
configuration. Publishing a package privately is what turns its name off upstream; there is no separate
deny-list to maintain.

## Removal semantics

PyPI distinguishes hiding a release from destroying it, and velodex keeps both:

- **Yank** ([PEP 592](https://peps.python.org/pep-0592/)) marks a file so resolvers skip it while exact-pin installs
  still succeed. It is reversible and is the right tool for a bad release that someone may already depend on.
- **Delete** removes uploaded records outright and is only allowed on `volatile` locals. For an overlay this
  un-shadows the upstream file. The content-addressed blob stays, since another index may reference the same digest.
- **Upstream files** cannot be modified on their mirror, so yanking or deleting one through an overlay records an
  override (`yanked` or `hidden`) on the overlay's local layer. The mirror's own route is untouched, the override
  applies wherever that local layer serves, and `restore` clears a hidden marker. This is how a broken upstream
  release is pulled from your resolvers within seconds, reversibly, without forking the mirror.

## The default topology

Out of the box velodex runs a pypi.org mirror plus an empty local, overlaid at `root/pypi`. One URL therefore serves
the whole public index, and the day you need to host a private package you add a token; nothing about the client
setup changes.

## In practice

- Build the topology: [compose overlays](@/guides/compose-overlays.md), [proxy a private mirror](@/guides/private-mirror.md)
- Publish into it: [publish](@/guides/publish.md); undo things: [yank and delete](@/guides/remove.md)
- The wire formats underneath: [standards](@/explanation/standards.md)
