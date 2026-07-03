+++
title = "From pypiserver"
description = "Keep the twine workflow, gain a real cache: pypiserver redirects misses upstream, velodex serves and keeps them."
weight = 3

[extra]
logos = ["logos/pypiserver.png"]
+++

[pypiserver](https://github.com/pypiserver/pypiserver) serves a directory of your own packages over the simple
API, with htpasswd-gated uploads. Its upstream story is a redirect: `--fallback-url` sends the client to
pypi.org for anything the directory lacks, and nothing comes back into the cache. The project also advertises
that it is looking for new maintainers.

## Why velodex

The redirect model means every machine still needs pypi.org access, every miss still pays full upstream latency,
and an upstream outage takes your installs down with it. velodex's mirror layer serves misses through itself and
keeps them: one egress point, [measured cold installs at upstream speed](@/explanation/performance.md), and
outage resilience. Your uploads then [shadow upstream names](@/explanation/indexes.md) instead of merely
coexisting with them.

## The renames

| pypiserver | velodex |
| ---------- | ------- |
| `pypi-server run -p 8080 ~/packages` | `velodex serve` |
| `http://host:8080/simple/` | `http://host:4433/{route}/simple/` |
| `-P htpasswd.txt -a update` | `upload_token` on the local index |
| `--fallback-url https://pypi.org/simple/` (redirect) | a mirror layer under the overlay (served and cached) |
| `--disable-fallback` | a local-only index, no mirror layer |
| `twine upload -r local dist/*` | the same command, pointed at the overlay route |

## Pitfalls

- pypiserver's per-action auth (`-a download,list,update`) has no counterpart: velodex authenticates uploads only,
  and reads are open to the network the port lives on.
- The package directory does not drop in: re-upload it once with twine
  (`for f in packages/*; do twine upload --repository-url http://host:4433/{route}/ "$f"; done`); velodex derives
  hashes and metadata server-side.
- If you relied on editing files in the package directory by hand, that workflow is gone; uploads are the write
  path.
