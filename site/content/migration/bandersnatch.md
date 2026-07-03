+++
title = "From bandersnatch"
description = "A mirror of everything versus a cache of what you use: when the full copy stops paying for its terabytes."
weight = 8

[extra]
logos = ["logos/pypa.png"]
+++

[bandersnatch](https://github.com/pypa/bandersnatch) is PyPI's official full-mirror client: a serial-based delta
sync of the whole index (or a filtered subset) onto your filesystem, served by a web server you bring. All of PyPI
is currently [41.8 TB](https://pypi.org/stats/), and the mirror can lag the index by
[up to about an hour](https://bandersnatch.readthedocs.io/en/latest/). It remains the right tool when policy
demands every package offline with no warm-up phase.

## Why velodex

Most mirrors exist to serve a working set thousands of times smaller than PyPI. A read-through cache stores
exactly that set, populated on first use at [no install-time penalty](@/explanation/performance.md), serves it
itself (no nginx layer, no name-normalization rewrite rules), stays as fresh as upstream's `Cache-Control`, and
hosts your private packages besides. bandersnatch's filter plugins narrow the terabytes; a cache removes the
guessing.

## The renames

| bandersnatch | velodex |
| ------------ | ------- |
| `bandersnatch mirror` on a timer | nothing: the cache populates on request |
| `/etc/bandersnatch.conf` `[mirror] directory` | `data_dir` |
| `allowlist_project` / requirements filters | the working set defines itself; lock files bound it |
| nginx serving `web/` | built-in server |
| `--force-check` full re-sync | delete `data_dir` (worst case: a refetch) |

## Pitfalls

- A cache has a warm-up: nothing is present until first requested. For an air gap, warm it on a connected network
  and [carry the data directory across](@/guides/air-gapped.md).
- "Every package, guaranteed present" is bandersnatch's contract, not velodex's; if an auditor requires the full
  index offline, stay with the mirror.
- bandersnatch's platform filters (drop Windows wheels, old Pythons) shrink a mirror; velodex has no equivalent
  because it only ever stores what was asked for.
