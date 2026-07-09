+++
title = "From proxpi"
description = "What proxpi and peryx share, where proxpi stays minimal, what peryx adds, and the one-line client change to move."
weight = 2
[extra]
logos = [ "logos/python.svg"]
+++

[proxpi](https://github.com/EpicWink/proxpi) is a [Flask](https://flask.palletsprojects.com/) caching proxy for the
simple API: one job, kept small. It caches index pages and files, speaks both JSON and HTML, serves
[PEP 658](https://peps.python.org/pep-0658/) metadata when the upstream advertises it, and runs anywhere Python does
(the bench runs it under [gunicorn](https://gunicorn.org/) with four workers). Its index cache lives in process memory,
its file cache defaults to a temporary directory, and it has no uploads and no private indexes.

## Comparison against peryx

### Overlap

- **Read-through caching** of the simple API, pages and files both.
- **JSON and HTML** simple responses.
- **PEP 658 metadata**: proxpi passes through `.metadata` siblings (with the PEP 714 `data-core-metadata` key) when the
  upstream offers them; peryx serves and synthesizes them.
- **Multiple upstreams**: proxpi's `PROXPI_EXTRA_INDEX_URLS` maps onto peryx cached indexes composed by a
  [virtual index](@/ecosystems/pypi/guides/compose-overlays.md).

### Extra: what proxpi does that peryx does not

- **Explicit cache invalidation.** proxpi exposes `DELETE /cache/{project}` and `DELETE /cache/list`. peryx has no
  invalidation endpoint; freshness follows the upstream's `Cache-Control`.
- **Size-capped eviction.** proxpi evicts least-frequently-used files once the cache passes `PROXPI_CACHE_SIZE` (5 GB
  default). peryx keeps everything it caches; the store grows with your working set.

### Missing: what peryx adds

- **Persistence you can rely on.** proxpi's index cache is per-process memory and its file cache defaults to a
  `tempfile.mkdtemp()` that is deleted on shutdown, so without a configured `PROXPI_CACHE_DIR` nothing survives a
  restart. Under four gunicorn workers the in-memory index cache is duplicated per worker and never shared. peryx is one
  process with a persistent content-addressed store shared by everything it does.
- **Private packages.** peryx hosts your own uploads [shadowing upstream names](@/core/indexes.md); proxpi is a proxy
  only, with no upload path.
- **Verified caching.** peryx checks each artifact against the digest its index page advertised before caching it.
- **No redirect to upstream.** When a download runs past `PROXPI_DOWNLOAD_TIMEOUT` (0.9 s default), proxpi redirects the
  client to pypi.org, so clients still need direct upstream access. peryx always serves through itself.

### Performance vs peryx

The [benchmark suite](@/core/performance.md) runs both from their published packages against the same workload. Cold and
warm installs through uv:

{{ bench(file="install-uv", only="peryx,proxpi") }}

The request workload, a swarm of resolvers reading full project pages, and the resource rows underneath it, show the
per-worker memory cost:

{{ bench(file="load", only="peryx,proxpi") }}

## How to migrate

The client change is one line: point your index URL at peryx. proxpi caches nothing you need to carry over; peryx
refills on first use. Map the environment knobs across:

| proxpi                                 | peryx                                                                                                       |
| -------------------------------------- | ----------------------------------------------------------------------------------------------------------- |
| `http://host:5000/index/`              | `http://host:4433/{route}/simple/`                                                                          |
| `PROXPI_INDEX_URL`                     | `cached = "https://pypi.org/simple/"` on a cached index                                                     |
| `PROXPI_EXTRA_INDEX_URLS`              | extra cached indexes, composed by a [virtual index](@/ecosystems/pypi/guides/compose-overlays.md)           |
| `PROXPI_INDEX_TTL`                     | upstream `Cache-Control`, with `cache_ttl_secs` as fallback ([how freshness works](@/core/architecture.md)) |
| `PROXPI_CACHE_DIR` (default: temp dir) | `data_dir` (persistent)                                                                                     |
| `PROXPI_CACHE_SIZE` eviction           | no size cap yet; the store grows with your working set                                                      |
| `curl -X DELETE /cache/{project}`      | wait out the freshness window, or restart with a clean `data_dir`                                           |

## Gotchas

- **Budget disk for your working set.** proxpi evicts past `PROXPI_CACHE_SIZE`; peryx currently keeps everything it
  caches.
- **No cache-invalidation endpoint.** Freshness follows the upstream's `Cache-Control` rather than a manual purge.
- **proxpi's TTL is a fixed number; peryx's is the upstream's.** proxpi expires on `PROXPI_INDEX_TTL` regardless of what
  pypi.org granted; peryx honors `Cache-Control` (`s-maxage`, then `max-age`) and falls back to `cache_ttl_secs`.
