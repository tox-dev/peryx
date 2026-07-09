+++
title = "Glossary (FAQ)"
description = "Plain-language answers to the cross-cutting terms velodex uses: index, cached/hosted/virtual, ecosystem, shadowing, upstream, publish, artifact."
weight = 6
+++

Every term velodex uses, defined as a question with a self-contained answer and a stable anchor, so the docs, the web
UI, and `--help` can all link to one place. No packaging background is assumed. Start at the top if you are new; jump by
anchor if you came here from a link.

## What is a package index? {#index}

An **index** is the list a package installer downloads from. When you run `pip install requests`, pip asks an index
"what files exist for `requests`, and where are they?", then downloads one. The public index for Python is
[pypi.org](https://pypi.org/). pip and uv call the address of an index its **index-url**.

A **registry** is the same idea under a different name. Some tools and ecosystems say "registry" where Python says
"index". This site says **index** throughout.

velodex is an index server: you run it, point your installer at it instead of pypi.org, and it answers those same
questions: from its cache, from packages you uploaded, or by asking an upstream index on your behalf.

## Why "index", not "repository"? {#index-not-repository}

Other tools call this a "repository" (Artifactory, Nexus) or a "registry" (npm, container tooling). They all mean the
same thing: a place a client resolves and downloads packages from. velodex standardizes on **index** because that is the
word Python's own tools and specifications use (`--index-url`, the "Simple **index**"), and because "repository"
collides with the source-control meaning (a git repo). When you read another tool's docs, read its "repository" or
"registry" as velodex's "index".

## What is an upstream? {#upstream}

An **upstream** is an index that velodex fetches from when it does not already have what a client asked for. pypi.org is
the usual upstream; a private Artifactory or GitLab registry can be one too. "Upstream" is a direction, not a role: it
is whatever sits above velodex in the fetch chain. A [cached](#roles) index has exactly one upstream.

## What does "publish" mean? {#publish}

To **publish** (or "upload") a package is to push a file you built up to an index so others can install it. For Python
you publish with [twine](https://twine.readthedocs.io/) or `uv publish`, which speak the standard upload API. In velodex
you publish into a [hosted](#roles) index. Publishing a name privately is also what turns that name off upstream; see
[shadowing](#shadowing).

## What is an artifact? {#artifact}

An **artifact** is an installable thing an index stores and serves. What one looks like depends on the
[ecosystem](#ecosystem): in the PyPI ecosystem it is a single file, a wheel or an sdist; in the OCI ecosystem it is a
small tree of content-addressed pieces: a manifest and its blobs. Each ecosystem's own page defines its artifact shape:
see [PyPI](@/ecosystems/pypi/_index.md) for wheels and sdists, and [OCI](@/ecosystems/oci/_index.md) for manifests,
blobs, and tags.

Whatever the shape, velodex stores every artifact by the sha256 hash of its bytes (**content-addressed**), so a file
needed by ten projects is stored once and is correct forever. A different file would have a different hash, and
therefore a different address.

## What is an ecosystem? {#ecosystem}

An **ecosystem** is a packaging format and the protocol that carries it: how clients ask for packages, how names and
versions are shaped, and what an [artifact](#artifact) looks like. PyPI (Python packages) is one ecosystem; OCI
(container images) is another.

velodex makes the ecosystem a first-class axis. Every index is a **role** (what it does) paired with an **ecosystem**
(which format it speaks). The two are independent, so the same three roles work for any ecosystem. A [virtual](#roles)
index may only combine members of the *same* ecosystem.

{% mermaid() %}
flowchart TB
  subgraph cached["role: cached"]
    c1["pypi"]
    c2["oci"]
  end
  subgraph hosted["role: hosted"]
    h1["pypi"]
    h2["oci"]
  end
  subgraph virtual["role: virtual"]
    v1["pypi"]
    v2["oci"]
  end
  classDef good fill:#009E73,stroke:#009E73,color:#ffffff
  class c1,h1,v1,c2,h2,v2 good
{% end %}

Each ecosystem fills a column across the three roles, and the roles work the same in every column. The
[capability matrix](@/core/capabilities.md) tracks what each supports.

## cached, hosted, virtual: what is the difference? {#roles}

These are the three **roles** an index can have. A role is what the index *does*; it is independent of the
[ecosystem](#ecosystem).

- **cached**: a read-through cache of one [upstream](#upstream). First request fetches from upstream and stores it;
  later requests come from local disk. (Was called a "mirror".)
- **hosted**: an authoritative store you [publish](#publish) to. There is no upstream; the packages are here because you
  uploaded them. (Was called a "local" index.)
- **virtual**: an ordered stack of other indexes served under one URL. Its member list is called `layers`, and it
  resolves them in order. (Was called an "overlay".)

A typical setup layers a hosted index in front of a cached index inside one virtual index, so clients use a single URL
and your own packages win. See [the index model](@/core/indexes.md) for the full treatment.

## What is shadowing, and why does my uploaded package win? {#shadowing}

**Shadowing** is what a [virtual](#roles) index does when two of its layers offer a file with the same name: the layer
listed first wins, and the later one is hidden ("shadowed"). velodex resolves the `layers` in order, keeps the first
occurrence of each filename, and critically resolves [cached](#roles) layers **last**. So a package you published into a
hosted layer always beats a same-named package from upstream.

{% mermaid() %}
flowchart TB
  req["client: GET simple/utils/"] --> V["virtual index<br/>resolve layers in order"]
  V -->|"1st: hosted layer"| L1["your upload<br/>utils-2.0 ✓ wins"]
  V -->|"2nd: another hosted layer"| L2["utils-1.0<br/>later, kept as older version"]
  V -->|"last: cached layer (pypi.org)"| L3["utils-99.0<br/>✗ shadowed, never served"]
  classDef good fill:#009E73,stroke:#009E73,color:#ffffff
  classDef warn fill:#D55E00,stroke:#D55E00,color:#ffffff
  class L1 good
  class L3 warn
{% end %}

This is a security control, not just a convenience. The common alternative (a private index in `--extra-index-url` with
pypi.org as fallback) lets anyone who registers your internal name on pypi.org with a higher version win the install.
That attack is called [dependency confusion](https://medium.com/@alex.birsan/dependency-confusion-4a5d60fec610).
Resolving cached layers last, server-side, closes it for every client at once: a name your hosted layer has is never
looked up upstream. Shadowing is per-filename, so you can override one broken wheel while the rest of a release still
comes from the cache.

## "cached" the role vs "cached" the file origin: which is which? {#cached-meanings}

The word **cached** appears in two places, and they are related but distinct:

- **cached, the [role](#roles)**: a kind of index, a read-through cache of one upstream. A property of the *index* you
  configure.
- **cached, the file provenance**: a label the web UI and search put on an individual *file* to say where it came from.
  A file is one of three:
  - **uploaded**: you [published](#publish) it into a hosted index.
  - **cached**: velodex fetched it from an upstream and stored it.
  - **override**: an uploaded file that shadows a same-named upstream file (see [shadowing](#shadowing)); the UI marks
    it so you can see the local decision winning.

So a *file* served from a *cached index* has provenance "cached"; a *file* you uploaded into a hosted layer of a virtual
index has provenance "uploaded", or "override" when it hides an upstream namesake. The role describes the index; the
provenance describes one file.

## Why do docker and podman accept velodex over plain HTTP? {#loopback-http}

Container clients refuse a plain-HTTP registry by default, assuming HTTPS, with one exception: a **loopback** address
(`localhost`, or anything in `127.0.0.0/8`) is treated as inherently trusted, so `docker pull localhost:4433/…` works
over HTTP with no configuration. This is why velodex is zero-config on the same host as the client, and why the standard
local registry (`registry:2`) runs on `localhost:5000`.

Two situations fall outside the loopback rule and need HTTPS or an explicit insecure setting:

- **Reaching velodex over the network**: a registry at a hostname or non-loopback IP is not trusted over HTTP.
- **Docker Desktop / a VM engine**: on macOS and Windows the engine runs in a VM, so the host's `localhost` is not the
  engine's `localhost`; you reach the host by another name, which is no longer loopback.

The production answer is to [serve HTTPS](@/core/serve-https.md) with a real or ACME certificate, which every client
trusts with no flag. For quick local testing you can instead pass `--tls-verify=false` (podman, crane) or add the
address to docker's `insecure-registries`.

## Related

- The full role model and topology: [the index model](@/core/indexes.md)
- What each ecosystem supports: [capability matrix](@/core/capabilities.md), [ecosystems](@/ecosystems/_index.md)
- The wire protocols named above: [standards](@/core/standards.md)
