+++
title = "From simpleindex"
description = "Route-per-project becomes virtual layers, and the redirects become a cache."
weight = 5
[extra]
logos = [ "logos/python.svg"]
+++

[simpleindex](https://github.com/uranusjr/simpleindex) routes simple-API requests by project-name pattern. A TOML file
maps each pattern to a local directory of files or an HTTP 302 redirect toward another index. Its scope covers routing
and local file serving.

## Why peryx

simpleindex suits route-only deployments. Redirected clients wait on the upstream, while peryx caches upstream responses
for machines behind one uplink. peryx also accepts [twine](https://twine.readthedocs.io/) uploads. A virtual index
applies hosted-first [dependency-confusion](@/core/indexes.md) protection to each published name without a route pattern
for that project.

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
