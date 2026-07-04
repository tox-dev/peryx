+++
title = "From proxpi"
description = "What proxpi and velodex share, where proxpi stays minimal, what velodex adds, and the one-line client change to move."
weight = 2
[extra]
logos = [ "logos/python.svg"]
+++

[proxpi](https://github.com/EpicWink/proxpi) is a [Flask](https://flask.palletsprojects.com/) caching proxy for the
simple API: one job, kept small. It caches index pages and files, speaks both JSON and HTML, serves
[PEP 658](https://peps.python.org/pep-0658/) metadata when the upstream advertises it, and runs anywhere Python does
(the bench runs it under [gunicorn](https://gunicorn.org/) with four workers). Its index cache lives in process memory,
its file cache defaults to a temporary directory, and it has no uploads and no private indexes.

## Comparison against velodex

### Overlap

- **Read-through caching** of the simple API, pages and files both.
- **JSON and HTML** simple responses.
- **PEP 658 metadata**: proxpi passes through `.metadata` siblings (with the PEP 714 `data-core-metadata` key) when the
  upstream offers them; velodex serves and synthesizes them.
- **Multiple upstreams**: proxpi's `PROXPI_EXTRA_INDEX_URLS` maps onto velodex mirror indexes composed by an
  [overlay](@/guides/compose-overlays.md).

### Extra: what proxpi does that velodex does not

- **Explicit cache invalidation.** proxpi exposes `DELETE /cache/{project}` and `DELETE /cache/list`. velodex has no
  invalidation endpoint; freshness follows the upstream's `Cache-Control`.
- **Size-capped eviction.** proxpi evicts least-frequently-used files once the cache passes `PROXPI_CACHE_SIZE` (5 GB
  default). velodex keeps everything it caches; the store grows with your working set.

### Missing: what velodex adds

- **Persistence you can rely on.** proxpi's index cache is per-process memory and its file cache defaults to a
  `tempfile.mkdtemp()` that is deleted on shutdown, so without a configured `PROXPI_CACHE_DIR` nothing survives a
  restart. Under four gunicorn workers the in-memory index cache is duplicated per worker and never shared. velodex is
  one process with a persistent content-addressed store shared by everything it does.
- **Private packages.** velodex hosts your own uploads [shadowing upstream names](@/explanation/indexes.md); proxpi is a
  proxy only, with no upload path.
- **Verified caching.** velodex checks each artifact against the digest its index page advertised before caching it.
- **No redirect to upstream.** When a download runs past `PROXPI_DOWNLOAD_TIMEOUT` (0.9 s default), proxpi redirects the
  client to pypi.org, so clients still need direct upstream access. velodex always serves through itself.

### Performance vs velodex

The [benchmark suite](@/explanation/performance.md) runs both from their published packages against the same workload.
Cold and warm installs through uv:

{{ bench(file="install-uv", only="velodex,proxpi") }}

The request workload, a swarm of resolvers reading full project pages, and the resource rows underneath it, show the
per-worker memory cost:

{{ bench(file="load", only="velodex,proxpi") }}

## How to migrate

The client change is one line: point your index URL at velodex. proxpi caches nothing you need to carry over; velodex
refills on first use. Map the environment knobs across:

| proxpi                                 | velodex                                                                                                            |
| -------------------------------------- | ------------------------------------------------------------------------------------------------------------------ |
| `http://host:5000/index/`              | `http://host:4433/{route}/simple/`                                                                                 |
| `PROXPI_INDEX_URL`                     | `mirror = "https://pypi.org/simple/"` on a mirror index                                                            |
| `PROXPI_EXTRA_INDEX_URLS`              | extra mirror indexes, composed by an [overlay](@/guides/compose-overlays.md)                                       |
| `PROXPI_INDEX_TTL`                     | upstream `Cache-Control`, with `cache_ttl_secs` as fallback ([how freshness works](@/explanation/architecture.md)) |
| `PROXPI_CACHE_DIR` (default: temp dir) | `data_dir` (persistent)                                                                                            |
| `PROXPI_CACHE_SIZE` eviction           | no size cap yet; the store grows with your working set                                                             |
| `curl -X DELETE /cache/{project}`      | wait out the freshness window, or restart with a clean `data_dir`                                                  |

## Gotchas

- **Budget disk for your working set.** proxpi evicts past `PROXPI_CACHE_SIZE`; velodex currently keeps everything it
  caches.
- **No cache-invalidation endpoint.** Freshness follows the upstream's `Cache-Control` rather than a manual purge.
- **proxpi's TTL is a fixed number; velodex's is the upstream's.** proxpi expires on `PROXPI_INDEX_TTL` regardless of
  what pypi.org granted; velodex honors `Cache-Control` (`s-maxage`, then `max-age`) and falls back to `cache_ttl_secs`.
