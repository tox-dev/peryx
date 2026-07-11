+++
title = "Performance"
description = "peryx as a Docker Hub pull-through cache next to the distribution reference registry and zot: cold and warm pulls, layer throughput, and a pull fleet."
weight = 2
+++

peryx streams an image's blobs to the client while teeing them into a content-addressed store, and concurrent pulls of
one uncached layer share a single upstream fetch. This page measures what that buys against the registries you would
otherwise put in front of Docker Hub, from a
[benchmark harness](https://github.com/tox-dev/peryx/tree/main/crates/peryx-bench) running every registry on
[one Apple Silicon machine](@/core/performance.md#the-machine-these-numbers-come-from). Pulls go through
[crane](https://github.com/google/go-containerregistry), so a real client handles the manifest walk and the bearer-token
dance; layer transfers are read in process over plain HTTP, because a subprocess per stream prices the client rather
than the registry.

## How this is measured, two ways

Every workload below is measured twice. The **against Docker Hub** table points each registry at `registry-1.docker.io`
and pulls for real: the cold row carries the actual upstream fetch (the network, Docker Hub's own latency, and the
proxy's store write), so it is marked network-bound and kept out of the regression gate, while the warm row is pure
cache serving. The **shielded** table swaps Docker Hub for a local pull-through cache, seeded once and shared by every
registry, which removes upstream variance and makes the run reproducible, isolating each registry's own serving cost.
Read together they separate what a first pull costs against the real internet from what a registry does with a layer it
already holds.

The shielded run is also the answer to Docker Hub's pull limit. A registry with no cache in front passes every client
pull straight through, so ten CI jobs pulling one image are ten pulls against your quota, and a rigorous benchmark that
restarts four registries on an empty cache each round burns through the hourly ceiling before it finishes. Put a cache
in front, which is what peryx is, and one upstream fetch serves everyone behind it, so the fleet's ten cold pulls
collapse to the single fetch the [fleet numbers](#a-pull-fleet) show.

## The field

Every party is a pull-through cache of Docker Hub, so the tables read against **direct**: a pull straight from
`registry-1.docker.io` with nothing in between, the baseline every ratio compares against.

| Registry                                                     | Stack                                                                                               | On a cold pull                                                                                    | Persisted cache                 |
| ------------------------------------------------------------ | --------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------- | ------------------------------- |
| [peryx](@/core/architecture.md)                              | one static Rust binary, async ([tokio](https://tokio.rs/)/[axum](https://github.com/tokio-rs/axum)) | streams each blob through, teeing into the store; concurrent misses for one layer share one fetch | content-addressed blobs on disk |
| [distribution](https://distribution.github.io/distribution/) | the reference registry (`registry:2`), Go, in `proxy` mode                                          | fetches and stores each blob, then serves it                                                      | filesystem, by repository       |
| [zot](https://zotregistry.dev/)                              | a Go registry with an on-demand `sync` extension                                                    | syncs the image from upstream when a manifest is first pulled                                     | filesystem, by repository       |
| direct ([Docker Hub](https://hub.docker.com/))               | no proxy, the client talks to Docker Hub                                                            | the full upstream pull, every time                                                                | none                            |

## Pulling images

The pull workload fetches six official images through each registry, cold (empty cache, every layer a miss) then warm
(the cache full, the client reset). Against Docker Hub the warm row is where a cache earns its place: peryx serves a
warm pull in **0.7 s** against 7.4 s to pull Docker Hub yourself, and ahead of distribution (2.7 s) and zot (6.6 s). The
cold row carries the real upstream fetch and is network-bound, yet peryx fills its cache in **6.7 s**, ahead of direct's
7.4 s despite also verifying and storing every layer: content-addressing fetches each base layer the six images share
exactly once. distribution pays 13.5 s and zot's on-demand sync far more, 51.2 s. You take the cold cost once per image,
and every pull after is the warm row.

{{ bench(file="pull") }}

Behind the mirror the same warm serving stands on its own, free of the network: peryx answers in **0.6 s**, clear of
distribution (2.7 s) and zot (6.7 s), and the cold fill settles to 3.2 s against direct's 2.8 s, the reproducible view
of the numbers above.

{{ bench(file="pull-mirror") }}

## Layer throughput

Once a layer is cached, how fast does it leave the registry? The throughput workload warms every registry with one large
layer (30 MB of `python:3.12-slim`), then streams it back, alone and under eight parallel readers. Warming first keeps
the row fair across designs: a pull-through proxy caches the layer on a blob request while a sync-based registry mirrors
it from the manifest, so pulling the image once gives every registry the layer to serve however its store holds it. All
three registries then stream from the page cache, and the interesting number is how many readers each one needs to
saturate the machine.

peryx serves a single stream at **6,838 MB/s** against zot's 2,772, and eight readers take it to 8,192. One reader is
already most of the way there, because peryx pipelines its reads: the next chunk is in flight while the current one goes
to the socket, so a lone client never waits on the disk. zot takes the kernel's zero-copy `sendfile` path, cheaper per
byte but serialized behind one reader's syscalls, so it needs eight overlapping readers to climb from 2,772 to 7,002
MB/s and reach the same neighbourhood.

That neighbourhood is the socket, not the registry. On this box a
[server that does nothing but write a buffer](@/core/performance.md#the-machine-these-numbers-come-from) hands a 30 MB
body to one loopback client at 10.2 GB/s. peryx's single stream is the same order of magnitude as that; zot's 2.8 GB/s
is not. Both registries leave the network-bound rows far behind, where Docker Hub itself manages 61 MB/s and
distribution 99.

{{ bench(file="image-throughput") }}

Behind the mirror the shape repeats and the gap widens slightly: peryx streams **7,164 MB/s** to one reader against
zot's 2,848, and at eight the two finish level, 8,747 to 8,694. The single-stream ratio, near 2.5x in both readings, is
the number to carry away; the eight-way figures are pressed against the machine and say more about the loopback than
about either registry.

{{ bench(file="image-throughput-mirror") }}

## A pull fleet

The fleet workload is ten clients pulling one image (`node:22-alpine`) at once, each with its own empty cache, exactly
like ten CI jobs landing on a runner pool together. Against Docker Hub it is where single-flight pays off most: peryx's
ten clients share the upstream fetches and finish cold in **2.2 s** and warm in **0.7 s**, against 6 to 12 s cold for
the others. It is the rate-limit story in one row: those ten pulls cost the upstream a single fetch through peryx, where
direct sends all ten to Docker Hub and stays at 6.4 s warm because it caches nothing.

{{ bench(file="parallel-pull") }}

Behind the mirror the shape survives without the network: peryx finishes cold in 1.1 s and warm in 0.6 s, still ahead of
the field, and the numbers stop moving with Docker Hub's weather.

{{ bench(file="parallel-pull-mirror") }}

## Every endpoint, not just the three a pull touches

`crane pull` needs three endpoints: the version check, a manifest, and a blob. The workloads above therefore never
measure the rest of what a registry serves, and an unmeasured endpoint is where a regression hides. Unlike a PyPI index,
an OCI registry's paths are fixed by the [distribution spec](https://github.com/opencontainers/distribution-spec), so
these rows compare like for like across the field.

{{ bench(file="image-endpoints-mirror") }}

peryx answers a cached manifest, by tag or by digest, in tens of microseconds, and a `HEAD` costs what the `GET` costs
without the body. That is worth reading carefully rather than as a win. A **tag is mutable**, so distribution and Docker
Hub ask upstream whether it still points where it did, on every request; peryx serves a tag from cache while it is fresh
and revalidates once its freshness window elapses, and zot re-checks its sync. The microseconds buy you a tag that can
be up to one freshness window stale, which is the trade a caching proxy exists to make. A manifest **by digest** is
immutable, and there the comparison is clean.

The revalidation itself is cheap: peryx asks what the tag points at with a `HEAD`, which answers with a digest and no
body, and only fetches the manifest when that digest has moved. So the freshness window is not buying a round trip, it
is buying the absence of one — and a burst of pulls of the same stale tag collapses into a single upstream check through
the single-flight gate. The window is [`cache_ttl_secs`](@/core/configuration.md), five minutes by default.

`tag list` used to be the one row peryx lost, and it lost it for a structural reason: a single-member proxy passed the
request straight to its upstream on every request, so the row measured a round trip to Docker Hub rather than a registry
serving something it holds. A tag list is mutable, which is why it asked — but that is what a freshness window is for.
It is now cached like a tag: trusted for [`cache_ttl_secs`](@/core/configuration.md), revalidated after, and answered
from the last list when the upstream cannot be reached, bounded by `max_stale_secs`. A burst of listings costs the
upstream one request, not one per client.

## Reproducing

Both readings come from the same harness, one straight against Docker Hub and one behind a local pull-through cache that
makes the run rate-limit-free and repeatable. See [run the benchmarks](@/contributing/benchmarking.md) for the commands
and the Docker Hub credentials they need.

## Related

- What the roles mean for containers: [the OCI ecosystem](@/ecosystems/oci/_index.md)
- Why the cold path keeps up and the warm path pulls ahead: [performance and methodology](@/core/performance.md)
- Run the container registry: [run a container registry](@/ecosystems/oci/guides/container-registry.md)
