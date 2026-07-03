+++
title = "From proxpi"
description = "The same one-line client change, plus persistence, uploads, and shadowing that proxpi leaves out."
weight = 2
[extra]
logos = [ "logos/python.svg"]
+++

[proxpi](https://github.com/EpicWink/proxpi) is a Flask caching proxy for the simple API: one job, kept small. It caches
index pages and files, speaks JSON and HTML, and runs anywhere Python does. It has no uploads, no private indexes, and
its index cache lives in process memory.

## Why velodex

{{ bench(file="install-uv") }}

{{ bench(file="load") }}

The larger differences are structural. proxpi's default file cache is a fresh temporary directory (`PROXPI_CACHE_DIR`
unset) that vanishes on restart, and its in-memory index cache means "use multiple threads instead of multiple
processes". velodex's cache is a persistent content-addressed store shared by everything the server does, artifacts
verify against their index digests before being cached, and the same binary hosts your private packages
[shadowing upstream](@/explanation/indexes.md).

## The renames

| proxpi                                 | velodex                                                                                                            |
| -------------------------------------- | ------------------------------------------------------------------------------------------------------------------ |
| `http://host:5000/index/`              | `http://host:4433/{route}/simple/`                                                                                 |
| `PROXPI_INDEX_URL`                     | `mirror = "https://pypi.org/simple/"` on a mirror index                                                            |
| `PROXPI_EXTRA_INDEX_URLS`              | extra mirror indexes, composed by an [overlay](@/guides/compose-overlays.md)                                       |
| `PROXPI_INDEX_TTL`                     | upstream `Cache-Control`, with `cache_ttl_secs` as fallback ([how freshness works](@/explanation/architecture.md)) |
| `PROXPI_CACHE_DIR` (default: temp dir) | `data_dir` (persistent)                                                                                            |
| `PROXPI_CACHE_SIZE` eviction           | no size cap yet; the store grows with your working set                                                             |
| `curl -X DELETE /cache/{project}`      | wait out the freshness window, or restart with a clean `data_dir`                                                  |

## Pitfalls

- proxpi evicts files past `PROXPI_CACHE_SIZE` (5 GB default); velodex currently keeps everything it caches. Budget disk
  for your working set.
- proxpi redirects the client to the upstream when a download takes longer than `PROXPI_DOWNLOAD_TIMEOUT`; velodex
  always serves through itself, so clients never need direct upstream access.
- There is no cache-invalidation endpoint; freshness follows the upstream's `Cache-Control`.
