+++
title = "From simpleindex"
description = "Route-per-project becomes virtual layers, and the redirects become a cache."
weight = 5
[extra]
logos = [ "logos/python.svg"]
+++

[simpleindex](https://github.com/uranusjr/simpleindex) routes simple-API requests by project-name pattern: a TOML file
maps each pattern to a local directory of files or an HTTP 302 redirect toward another index. Doing nothing else is its
design: no caching, no uploads, no storage.

## Why peryx

If simpleindex covers your need, it is admirably small. The reasons people outgrow it map one-to-one onto what peryx
adds: redirected clients still need (and wait on) the upstream, so a cache helps every machine behind one uplink; a
directory of files needs a separate upload workflow, so twine support helps; and pattern routing protects against
[dependency confusion](@/core/indexes.md) only as well as the patterns you remember to write, where a virtual index's
hosted-first shadowing is the default for every name you publish.

## The renames

| simpleindex                               | peryx                                                |
| ----------------------------------------- | ---------------------------------------------------- |
| `simpleindex ./configuration.toml`        | `peryx serve --config peryx.toml`                    |
| route `source = "http"` (302 to an index) | a cached layer (fetched, verified, cached)           |
| route `source = "path"` (local directory) | a hosted index, populated by `twine upload`          |
| per-project route patterns                | virtual resolution: hosted layers first, cached last |
| `[server] host / port`                    | `host` / `port`                                      |

## Pitfalls

- simpleindex's explicit routing can send *different projects to different upstreams*; peryx's virtual index resolves
  every project through the same layer order. Model per-project pinning as separate routes (one virtual index per
  upstream) if you need it.
- Hosted files must be re-uploaded once; there is no directory-import.
