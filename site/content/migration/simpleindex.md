+++
title = "From simpleindex"
description = "Route-per-project becomes overlay layers, and the redirects become a cache."
weight = 5
[extra]
logos = [ "logos/python.svg"]
+++

[simpleindex](https://github.com/uranusjr/simpleindex) routes simple-API requests by project-name pattern: a TOML file
maps each pattern to a local directory of files or an HTTP 302 redirect toward another index. Doing nothing else is its
design: no caching, no uploads, no storage.

## Why velodex

If simpleindex covers your need, it is admirably small. The reasons people outgrow it map one-to-one onto what velodex
adds: redirected clients still need (and wait on) the upstream, so a cache helps every machine behind one uplink; a
directory of files needs a separate upload workflow, so twine support helps; and pattern routing protects against
[dependency confusion](@/explanation/indexes.md) only as well as the patterns you remember to write, where an overlay's
local-first shadowing is the default for every name you publish.

## The renames

| simpleindex                               | velodex                                             |
| ----------------------------------------- | --------------------------------------------------- |
| `simpleindex ./configuration.toml`        | `velodex serve --config velodex.toml`               |
| route `source = "http"` (302 to an index) | a mirror layer (fetched, verified, cached)          |
| route `source = "path"` (local directory) | a local index, populated by `twine upload`          |
| per-project route patterns                | overlay resolution: local layers first, mirror last |
| `[server] host / port`                    | `host` / `port`                                     |

## Pitfalls

- simpleindex's explicit routing can send *different projects to different upstreams*; velodex's overlay resolves every
  project through the same layer order. Model per-project pinning as separate routes (one overlay per upstream) if you
  need it.
- Local files must be re-uploaded once; there is no directory-import.
