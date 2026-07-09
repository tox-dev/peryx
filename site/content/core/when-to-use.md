+++
title = "When to use peryx (and when not)"
description = "The problems a read-through cache and private index solves for any ecosystem, and the honest list of problems it does not."
weight = 0
+++

peryx sits between your clients and the indexes they download from: Python installers and pypi.org, container clients
and Docker Hub, whatever the ecosystem. That position solves a specific set of problems. If yours is on the first list,
peryx is a one-binary answer; if it is on the second, use the tool named there instead.

## Use peryx when…

### Your CI re-downloads the same artifacts all day

Container-based CI rebuilds environments from scratch on every run, so a busy monorepo pulls the same wheels from
pypi.org and the same base images from Docker Hub hundreds of times a day. That is slow for you and expensive for the
upstream. PyPI's operators
[ask CI-heavy users to run local mirrors](https://discuss.python.org/t/draft-pep-pypi-cost-solutions-ci-mirrors-containers-and-caching-to-scale/3681),
and Docker Hub enforces pull-rate limits that stall unauthenticated builds. A read-through cache fixes both with a
one-line change: point the client's index or registry at peryx and every job after the first serves from local disk.
The [usage counters](@/core/monitor.md) show what it saves you.

### You install private artifacts next to public ones

The common pattern (a private index alongside the public one as fallback) is how
[dependency confusion](https://medium.com/@alex.birsan/dependency-confusion-4a5d60fec610) works: the client happily
takes a same-named, higher-versioned artifact from the public side. It is a real attack; it compromised
[PyTorch nightly users](https://pytorch.org/blog/compromised-nightly-dependency/) and earned one researcher bug bounties
from 35 companies, and the same name-collision risk applies to container image names. peryx's virtual indexes answer
it server-side: your uploads shadow upstream artifacts with the same name, for every client at once.
[The index model](@/core/indexes.md) explains the mechanics.

### The upstream being down should not stop your team

When an upstream or its CDN degrades, every build that depends on it turns red. peryx serves stale index pages when
the upstream errors, and serves cached artifacts forever because they are immutable, addressed by their hash. An outage
upstream degrades to "no brand-new releases for a while" instead of "nobody can deploy".

### You need an internal index in a restricted network

Full mirrors are the traditional air-gap answer, and a full public mirror is enormous: a complete PyPI mirror is
[double-digit terabytes](https://github.com/pypa/bandersnatch/issues/1105) and growing, and image registries are larger
still. A read-through cache is a partial mirror that populates itself on first use, or ahead of time with
`peryx mirror sync`. On a network with controlled egress, peryx is the one approved path to the upstream; for a true
air gap, sync the working set on a connected network and carry the data directory across.

### Artifacts are big and your bandwidth is not

A CUDA-enabled torch wheel is measured in gigabytes, and a GPU container image larger. A classroom, a Raspberry Pi
fleet, or a team behind one uplink downloads each once through peryx and then never again: the store is
content-addressed, so one copy serves every project, tag, and machine, and a layer shared across images is stored once.
Where an ecosystem offers a metadata shortcut (Python's `.metadata` siblings, a registry's manifest before its blobs),
peryx speaks it, so a resolver reads what it needs without pulling the whole artifact.

### You would run Artifactory or Nexus for one format alone

The universal artifact managers do this job, priced and sized for doing every job: a JVM, gigabytes of RAM, and license
costs that scale with usage. If you need one or two ecosystems, peryx is one static binary, one TOML file, and one
data directory.

## …and when not

- **You need a full mirror protocol or delta mirror tooling.** `peryx mirror sync --mode all` can walk an upstream,
  but ecosystem-specific full-mirror tools own that workflow and its operational conventions, such as
  [bandersnatch](https://github.com/pypa/bandersnatch) for PyPI.
- **You need an ecosystem peryx does not serve yet.** peryx serves PyPI and OCI (container images); its architecture
  is per-ecosystem (an index is a role paired with an ecosystem; see [ecosystems](@/ecosystems/_index.md)) and more
  formats plug in as drivers, but those are not built yet. If you need one right now, that is
  [Artifactory](https://jfrog.com/artifactory/) and [Nexus](https://www.sonatype.com/products/nexus-repository)
  territory.
- **You need high availability or replication.** peryx is one process with local state. Run it per site or per cluster
  (each instance warms independently), but there is no primary/replica story yet.
- **You need per-user authentication and read ACLs.** Today's auth is one upload token per hosted index; reads are open
  to whoever can reach the port. Put it behind your network boundary or a reverse proxy that handles identity.
- **You need a build farm** the way [piwheels](https://www.piwheels.org/) compiles wheels for Raspberry Pi. peryx
  serves what upstream has; it does not build anything.

## In practice

- Get running: [getting started](@/core/getting-started.md), then your [ecosystem](@/ecosystems/_index.md).
- Host private artifacts safely: [the index model](@/core/indexes.md).
- Understand the machinery: [architecture](@/core/architecture.md).
