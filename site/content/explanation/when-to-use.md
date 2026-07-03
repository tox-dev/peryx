+++
title = "When to use velodex (and when not)"
description = "The problems a read-through cache and private index solves, and the honest list of problems it does not."
weight = 0
+++

velodex sits between Python installers and the indexes they download from. That position solves a specific set of
problems. If yours is on the first list, velodex is a one-binary answer; if it is on the second, use the tool named
there instead.

## Use velodex when…

### Your CI re-downloads the same packages all day

Container-based CI rebuilds environments from scratch on every run, so a busy monorepo pulls the same wheels from
pypi.org hundreds of times a day. That is slow for you and expensive for PyPI, whose operators
[ask CI-heavy users to run local mirrors](https://discuss.python.org/t/draft-pep-pypi-cost-solutions-ci-mirrors-containers-and-caching-to-scale/3681).
A read-through cache fixes it with a one-line change: set `UV_INDEX_URL` or `PIP_INDEX_URL` in the runner environment
and every job after the first serves from local disk. Teams that made this change report multiply faster builds and tens
of gigabytes of external traffic gone; the [CI guide](@/guides/ci-cache.md) shows the setup, and the
[usage counters](@/guides/monitor.md) show what it saves you.

### You install private packages next to public ones

The common pattern, a private index in `--extra-index-url` with pypi.org as fallback, is how
[dependency confusion](https://medium.com/@alex.birsan/dependency-confusion-4a5d60fec610) works: pip happily takes a
same-named, higher-versioned package from the public side. This is not theoretical; it compromised
[PyTorch nightly users](https://pytorch.org/blog/compromised-nightly-dependency/) and earned one researcher bug bounties
from 35 companies. velodex's overlay indexes answer it server-side: your uploads shadow upstream files with the same
name, for every client; pip, uv, and poetry alike keep a single `index-url`. [The index model](@/explanation/indexes.md)
explains the mechanics.

### pypi.org being down should not stop your team

When PyPI or its CDN degrades, every build that depends on it turns red. velodex serves stale pages when the upstream
errors and serves cached artifacts forever (they are immutable), so an outage upstream degrades to "no brand-new
releases for a while" instead of "nobody can deploy".

### You need an internal index in a restricted network

Full mirrors are the traditional air-gap answer, and a full PyPI mirror is
[double-digit terabytes](https://github.com/pypa/bandersnatch/issues/1105) and growing. A read-through cache is a
partial mirror that populates itself with exactly the packages you use, at the moment you first use them. On a network
with controlled egress, velodex is the one approved path to PyPI; for a true air gap, warm the cache on a connected
network and carry the data directory across. The [air-gap guide](@/guides/air-gapped.md) covers both.

### Wheels are big and your bandwidth is not

A CUDA-enabled torch wheel is measured in gigabytes. A classroom, a Raspberry Pi fleet, or a GPU team behind one uplink
downloads it once through velodex and then never again: the store is content-addressed, so one copy serves every
project, tag, and student. Because velodex speaks PEP 658, resolvers also stop downloading wheels just to read their
dependency metadata; a few kilobytes of `.metadata` replaces each candidate download.

### You would run Artifactory or Nexus for Python alone

The universal artifact managers do this job, priced and sized for doing every job: a JVM, gigabytes of RAM, and license
costs that scale with usage. If Python packages are the only artifacts you need, velodex is one static binary, one TOML
file, and one data directory.

## …and when not

- **You need a complete offline PyPI**, every package and every release, without a warm-up phase. That is a full mirror;
  run [bandersnatch](https://github.com/pypa/bandersnatch) and budget the terabytes.
- **You need one registry for many ecosystems**: npm, Maven, Docker images, Debian packages. That is
  [Artifactory](https://jfrog.com/artifactory/) and [Nexus](https://www.sonatype.com/products/nexus-repository)
  territory; velodex only speaks Python's index protocols.
- **You need high availability or replication.** velodex is one process with local state. Run it per site or per cluster
  (each instance warms independently), but there is no primary/replica story yet.
- **You need per-user authentication and read ACLs.** Today's auth is one upload token per local index; reads are open
  to whoever can reach the port. Put it behind your network boundary or a reverse proxy that handles identity.
- **You need build farms for source distributions** the way [piwheels](https://www.piwheels.org/) provides for Raspberry
  Pi. velodex serves what upstream has; it does not compile anything.

## In practice

- Set up the cache: [getting started](@/tutorials/getting-started.md), [CI guide](@/guides/ci-cache.md)
- Host private packages safely: [compose overlays](@/guides/compose-overlays.md), [publish](@/guides/publish.md)
- Understand the machinery: [architecture](@/explanation/architecture.md), [the index model](@/explanation/indexes.md)
