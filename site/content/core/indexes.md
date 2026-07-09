+++
title = "The index model"
description = "Cached, hosted, and virtual indexes across ecosystems: how composition works, why shadowing is the dependency-confusion fix, and what removal means."
weight = 3
+++

An index server earns its keep the day you have a package that must not come from the public internet. This page
explains peryx's answer. An **index** is the list of packages a client installs from (pip and uv call it an index-url);
a **registry** is the same idea under a different name. peryx builds every index from two independent choices, a
**role** (what the index does) and an **ecosystem** (which packaging format it speaks), and one composition rule. That
rule is a security control before it is a convenience.

## Two axes: role and ecosystem

Every index peryx serves is a triple: a **role**, an **ecosystem**, and a **key** (its name). The two axes are
independent.

- **role** is what the index does. There are three:
  - **cached** is a read-through cache of one upstream index. "Upstream" means the index peryx fetches from, e.g.
    pypi.org. On a first request peryx fetches from upstream, stores the result, and serves it; later requests come from
    local disk. (This was called a "mirror".)
  - **hosted** is an authoritative store you upload to. Nothing upstream; the packages live here because you published
    them. (This was called a "local" index.)
  - **virtual** is an ordered aggregation of other indexes served under one URL. Its member list is called `layers`.
    (This was called an "overlay".)
- **ecosystem** is which packaging format the index speaks: **pypi** and **oci** today. It fixes the wire protocol (the
  PyPI Simple API, the OCI `/v2/` distribution API) and the artifact shapes (wheels and sdists; manifests and blobs).
  See [ecosystems](@/ecosystems/_index.md) and the [standards](@/core/standards.md) each one implements.

The axes are orthogonal at creation but coupled at aggregation: a virtual index may only combine members of the **same
ecosystem**. The roles below work the same in every ecosystem; the diagrams use a PyPI example, and each ecosystem's
page shows the same shapes in its own protocol.

## Prior art

The index servers teams run in production converged on the same role shape, and peryx adopts it:

- **Artifactory** aggregates *local* and *remote* repositories into a *virtual* repository behind one URL, with a
  default deployment target for writes and local-before-remote resolution.
- **Nexus** groups *hosted* and *proxy* repositories the same way; the member order decides who wins, and the
  documentation recommends hosted first.

The shared pattern: a read-through cache primitive, a writable hosted primitive, and an ordered composition served at
one URL where your own content wins over upstream. peryx names these cached, hosted, and virtual.

## peryx's three roles

- A **cached** index proxies and caches one upstream, with its own credentials.
- A **hosted** index stores uploads; `upload_token` gates writes and `volatile` gates deletion.
- A **virtual** index serves an ordered list of other indexes under one route. Resolution is first-match per filename;
  versions union. Uploads land in the virtual index's designated hosted layer. A layer can be another virtual index,
  which gives inheritance chains.

{% mermaid() %}
flowchart LR
  req["GET simple/utils/"] --> virtual["virtual root/pypi"]
  virtual -->|"1st: hosted layer"| hosted["your uploads<br/>utils-2.0 ✓"]
  virtual -->|"2nd: cached layer"| cached["pypi.org cache<br/>utils-9.9 ✗ shadowed"]
  classDef good fill:#009E73,stroke:#009E73,color:#ffffff
  classDef warn fill:#D55E00,stroke:#D55E00,color:#ffffff
  class hosted good
  class cached warn
{% end %}

Filename-level (rather than project-level) shadowing means you can override one broken wheel of an upstream release
while its sdist and its other wheels continue to come from the cached layer.

## Why shadowing is a security control

The usual way to mix private and public packages is client-side: a private index in `--extra-index-url`, pypi.org as the
default. pip treats both indexes as equals and installs whichever offers the higher version. Anyone who registers your
internal package's name on pypi.org with version `99.0` now wins the race. This is
[dependency confusion](https://medium.com/@alex.birsan/dependency-confusion-4a5d60fec610), the technique that
compromised [PyTorch's nightly channel](https://pytorch.org/blog/compromised-nightly-dependency/) and, in its original
disclosure, three dozen major companies. Client-side mitigations exist (uv's `explicit` index pinning, for one) but must
be repeated in every project, for every tool, forever.

A virtual index moves the decision server-side. The client has one `index-url` and no fallback; the virtual index's
hosted layer is consulted first; a name that exists in the hosted layer never falls through to the cached layer. The
guarantee holds for pip, uv, poetry, and whatever comes next, because it lives where the indexes meet rather than in
each client's configuration. Publishing a package privately is what turns its name off upstream; there is no separate
deny-list to maintain.

## Removal semantics

peryx distinguishes hiding an artifact from destroying it, and keeps both (the PyPI names appear here; each ecosystem
maps them to its own protocol):

- **Yank** ([PEP 592](https://peps.python.org/pep-0592/)) marks a file so resolvers skip it while exact-pin installs
  still succeed. It is reversible and is the right tool for a bad release that someone may already depend on.
- **Delete** removes uploaded records outright and is only allowed on `volatile` hosted indexes. For a virtual index
  this un-shadows the upstream file. The content-addressed blob stays, since another index may reference the same
  digest.
- **Upstream files** cannot be modified on their cached index, so yanking or deleting one through a virtual index
  records an override (`yanked` or `hidden`) on the virtual index's hosted layer. The cached index's own route is
  untouched, the override applies wherever that hosted layer serves, and `restore` clears a hidden marker. This is how a
  broken upstream release is pulled from your resolvers within seconds, reversibly, without forking the cache.

## The default topology

Out of the box peryx runs one trio per ecosystem: a cached index of the public upstream, an empty hosted index, and a
virtual index combining them. For PyPI that is a pypi.org cache behind `root/pypi`; for OCI, a Docker Hub cache behind
`root/oci`. Each virtual URL serves the whole public index for its ecosystem, and the day you need to host a private
artifact you add a token to that ecosystem's hosted index; nothing about the client setup changes.

## In practice

- Build the topology in your ecosystem: [PyPI](@/ecosystems/pypi/_index.md), [OCI](@/ecosystems/oci/_index.md)
- The vocabulary in one place: [glossary](@/core/glossary.md)
- The wire formats underneath: [standards](@/core/standards.md)
