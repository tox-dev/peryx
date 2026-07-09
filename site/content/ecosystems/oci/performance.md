+++
title = "Performance"
description = "velodex as a Docker Hub pull-through cache next to the distribution reference registry and zot: cold and warm pulls, layer throughput, and a pull fleet."
weight = 2
+++

velodex streams an image's blobs to the client while teeing them into a content-addressed store, and concurrent pulls of
one uncached layer share a single upstream fetch. This page measures what that buys against the registries you would
otherwise put in front of Docker Hub, from a
[benchmark harness](https://github.com/tox-dev/velodex/tree/main/crates/velodex-bench) driving
[crane](https://github.com/google/go-containerregistry) against each registry on one Apple Silicon laptop.

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
in front, which is what velodex is, and one upstream fetch serves everyone behind it, so the fleet's ten cold pulls
collapse to the single fetch the [fleet numbers](#a-pull-fleet) show. Run both readings with
`cargo run --release -p velodex-bench -- --ecosystem oci` and again with `--mirror`.

## The field

Every party is a pull-through cache of Docker Hub, so the tables read against **direct**: a pull straight from
`registry-1.docker.io` with nothing in between, the baseline every ratio compares against.

| Registry                                                     | Stack                                                                                               | On a cold pull                                                                                    | Persisted cache                 |
| ------------------------------------------------------------ | --------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------- | ------------------------------- |
| [velodex](@/core/architecture.md)                            | one static Rust binary, async ([tokio](https://tokio.rs/)/[axum](https://github.com/tokio-rs/axum)) | streams each blob through, teeing into the store; concurrent misses for one layer share one fetch | content-addressed blobs on disk |
| [distribution](https://distribution.github.io/distribution/) | the reference registry (`registry:2`), Go, in `proxy` mode                                          | fetches and stores each blob, then serves it                                                      | filesystem, by repository       |
| [zot](https://zotregistry.dev/)                              | a Go registry with an on-demand `sync` extension                                                    | syncs the image from upstream when a manifest is first pulled                                     | filesystem, by repository       |
| direct ([Docker Hub](https://hub.docker.com/))               | no proxy, the client talks to Docker Hub                                                            | the full upstream pull, every time                                                                | none                            |

## Pulling images

The pull workload fetches six official images through each registry, cold (empty cache, every layer a miss) then warm
(the cache full, the client reset). Against Docker Hub the warm row is where a cache earns its place: velodex serves a
warm pull in **0.5 s** against 7.2 s to pull Docker Hub yourself, and ahead of distribution (2.4 s) and zot (6.4 s). The
cold row carries the real upstream fetch and is network-bound, yet velodex fills its cache in **6.7 s**, level with
direct's 7.3 s despite also verifying and storing every layer: content-addressing fetches each base layer the six images
share exactly once. distribution pays 12.9 s and zot's on-demand sync far more, 51.3 s. You take the cold cost once per
image, and every pull after is the warm row.

{{ bench(file="pull") }}

Behind the mirror the same warm serving stands on its own, free of the network: velodex answers in **2.2 s**, level with
distribution (2.6 s) and clear of zot (6.4 s), and the cold fill settles to 3.0 s against direct's 2.7 s, the
reproducible view of the numbers above.

{{ bench(file="pull-mirror") }}

## Layer throughput

Once a layer is cached, how fast does it leave the registry? The throughput workload warms every registry with one large
layer (30 MB of `python:3.12-slim`), then streams it back, alone and under eight parallel readers. Warming first keeps
the row fair across designs: a pull-through proxy caches the layer on a blob request while a sync-based registry mirrors
it from the manifest, so pulling the image once gives every registry the layer to serve however its store holds it. All
three registries stream from disk. Eight-way velodex and zot run level at the front, **803** and **795 MB/s**, both far
over distribution's 148 and direct's 88. zot reaches it through the kernel's `sendfile` path, copying bytes straight
from the page cache to the socket; velodex pipelines its own reads so the disk read runs ahead of the socket write,
matching zot's throughput despite the userspace copy it still pays. Single-stream the order flips: zot's zero-copy leads
at 181 against velodex's 68, where velodex's per-request setup shows through with only one reader to amortize it. These
are the noisiest rows in the suite: each transfer is a short `crane` subprocess, so single-stream numbers are dominated
by process overhead and the spreads run wide; read them as broad strokes, not to the digit.

{{ bench(file="image-throughput") }}

Behind the mirror the ordering holds, zot's zero-copy path ahead and velodex next at **576 MB/s** eight-way, a stride
behind zot's 593; the wide single-stream spreads hold with it, the honest read on a subprocess-bound micro-workload.

{{ bench(file="image-throughput-mirror") }}

## A pull fleet

The fleet workload is ten clients pulling one image (`node:22-alpine`) at once, each with its own empty cache, exactly
like ten CI jobs landing on a runner pool together. Against Docker Hub it is where single-flight pays off most:
velodex's ten clients share the upstream fetches and finish cold in **2.2 s** and warm in **0.6 s**, against 6–9 s cold
for the others. It is the rate-limit story in one row: those ten pulls cost the upstream a single fetch through velodex,
where direct sends all ten to Docker Hub and stays at 6.0 s warm because it caches nothing.

{{ bench(file="parallel-pull") }}

Behind the mirror the shape survives without the network: velodex finishes cold in 1.5 s and warm in 0.9 s, still ahead
of the field, and the numbers stop moving with Docker Hub's weather.

{{ bench(file="parallel-pull-mirror") }}

## Reproducing

With the repository checked out and Docker running, regenerate both readings:

```shell
cargo run --release -p velodex-bench -- --ecosystem oci            # against Docker Hub
cargo run --release -p velodex-bench -- --ecosystem oci --mirror   # shielded, reproducible
```

Set `DOCKERHUB_USERNAME` and a read-only [access token](https://docs.docker.com/security/for-developers/access-tokens/)
in `DOCKERHUB_TOKEN` before the first form: the harness threads them into every registry and crane, and an authenticated
account lifts the pull ceiling an anonymous run hits mid-comparison. The `--mirror` form stands a local pull-through
cache in front of Docker Hub and points every registry at it, so it is rate-limit-free and repeatable; without it the
cold rows carry the real upstream fetch and are marked network-bound, kept out of the regression gate.

## Related

- What the roles mean for containers: [the OCI ecosystem](@/ecosystems/oci/_index.md)
- Why the cold path keeps up and the warm path pulls ahead: [performance and methodology](@/core/performance.md)
- Run the container registry: [run a container registry](@/ecosystems/oci/guides/container-registry.md)
