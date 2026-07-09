+++
title = "From bandersnatch"
description = "A mirror of everything versus a cache of what you use: when the full copy stops paying for its terabytes."
weight = 8
[extra]
logos = [ "logos/pypa.png"]
+++

[bandersnatch](https://github.com/pypa/bandersnatch) is PyPI's official full-mirror client: a serial-based delta sync of
the whole index (or a filtered subset) onto your filesystem, served by a web server you bring. All of PyPI is currently
[41.8 TB](https://pypi.org/stats/), and the mirror can lag the index by
[up to about an hour](https://bandersnatch.readthedocs.io/en/latest/). It remains the right tool when policy demands
every package offline with no warm-up phase.

## Why peryx

Most mirrors exist to serve a working set thousands of times smaller than PyPI. A read-through cache stores that set,
populated on first use at [no install-time penalty](@/core/performance.md) or ahead of time with `peryx mirror sync`,
serves it itself (no nginx layer, no name-normalization rewrite rules), stays as fresh as upstream's `Cache-Control`,
and hosts your private packages besides. bandersnatch's filter plugins narrow the terabytes; requirements-based sync
removes the guessing when clients already install from locks or requirements files.

## The renames

| bandersnatch                                  | peryx                                                                      |
| --------------------------------------------- | ---------------------------------------------------------------------------- |
| `bandersnatch mirror` on a timer              | `peryx mirror sync <index>` for prefetch, or read-through population       |
| `/etc/bandersnatch.conf` `[mirror] directory` | `data_dir`                                                                   |
| `allowlist_project` / requirements filters    | `[index.policy]` allow/block rules plus `[index.prefetch]` selectors         |
| platform and Python wheel filters             | `[index.policy]` wheel rules for serving; `[index.prefetch]` for mirror sync |
| nginx serving `web/`                          | built-in server                                                              |
| `--force-check` full re-sync                  | `peryx mirror verify <index>` plus targeted purge/resync                   |

## Pitfalls

- Read-through mode has a warm-up: nothing is present until requested. For an air gap, run `peryx mirror sync` on a
  connected network and [carry the data directory across](@/ecosystems/pypi/guides/air-gapped.md).
- `--mode all` walks the upstream root Simple index, but peryx does not implement PyPI's serial mirror protocol. If an
  auditor requires that protocol, stay with bandersnatch.
- Policy applies when peryx serves, mirrors, caches, or accepts an upload. Prefetch filters decide what
  `peryx mirror sync` brings in ahead of demand.
