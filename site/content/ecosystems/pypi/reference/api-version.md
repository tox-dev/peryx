+++
title = "The advertised Simple API version"
description = "How peryx derives the meta.api-version it serves from the upstream it proxies: 1.4 when the upstream promises PEP 700's versions and size (1.1+), 1.0 otherwise, the weakest layer for a virtual index, and JSON-only."
weight = 5
+++

Every Simple page peryx serves carries a version: `meta.api-version` in the [PEP 691](https://peps.python.org/pep-0691/)
JSON, `pypi:repository-version` in the [PEP 503](https://peps.python.org/pep-0503/) HTML `<meta>`. peryx does not stamp
a fixed number. It derives the version from what the upstream it proxies declared, so the advertised version never
promises a field the re-served payload can omit.

## The rule

peryx reads the version the upstream declared for the project, then maps it to what it serves:

| Upstream declares                                                     | peryx serves | Why                                                         |
| --------------------------------------------------------------------- | ------------ | ----------------------------------------------------------- |
| `1.1`, `1.2`, `1.3`, `1.4`, `1.5`, … (minor ≥ 1)                      | `1.4`        | PEP 700 makes `versions` and per-file `size` mandatory here |
| `1.0`                                                                 | `1.0`        | PEP 691 mandates neither field                              |
| nothing (a bare PEP 503 HTML index, or JSON that omits `api-version`) | `1.0`        | promises neither field                                      |
| a major other than `1` (`2.0`, …)                                     | rejected     | unsupported major; the upstream page is not served          |
| a version that does not parse (`1.x`, `abc`)                          | rejected     | invalid version; the upstream page is not served            |

`1.4` is peryx's own ceiling: the highest version it implements. The threshold that decides between the ceiling and the
base is [PEP 700](https://peps.python.org/pep-0700/)'s, minor version `1`. Above it, every guarantee through `1.4` is
one peryx meets by passing the upstream's fields through, so it advertises the full ceiling rather than echoing the
exact minor the upstream sent.

## What the version guarantees

PEP 700 raised the Simple API to `1.1` and made two fields mandatory in the JSON serialization:

- **`versions`**: a top-level array of every release version of the project.
- **`size`**: an integer byte count on every file entry.

A page that advertises `1.1` or higher promises both are present; `1.0` promises neither. peryx advertises `1.4` only
when the upstream declared `1.1+`, where those fields are guaranteed in the bytes it re-serves, and falls back to `1.0`
otherwise.

## Virtual indexes take the weakest layer

A virtual index merges the project pages of its layers, and it is only as capable as its least capable layer. peryx
starts the merged page at its `1.4` ceiling and drops it to `1.0` the moment any layer that resolved the project serves
`1.0`. A single pre-PEP 700 layer therefore caps the merged page at `1.0`, because the merged payload can no longer
guarantee `versions` and `size` for every file.

The cap is per project. A layer only lowers the version when it returns a page for the requested project; a layer that
does not carry the project has no say in its version.

## JSON only; HTML is unaffected

PEP 700 changes the JSON serialization alone. The HTML serialization defines no `versions` array and no per-file `size`,
so it carries none of PEP 700's guarantees at any version number. The derivation sets the version on the served `meta`,
which both serializations render, but the honesty concern is JSON-only: an HTML page has no PEP 700 field to
over-advertise.

## What it does not do

- It does not synthesize `versions` or `size` to reach `1.4`. When the upstream promises neither, peryx lowers the
  version rather than inventing the fields.
- It does not echo the upstream's exact minor. Any `1.1+` maps to `1.4`, peryx's ceiling, not to the number the upstream
  sent.
- It does not serve an unsupported major or an unparseable version. Those are errors, not a page.

## Related

- Watch the version follow two upstreams end to end:
  [watch the advertised version follow the upstream](@/ecosystems/pypi/tutorials/api-version.md)
- Track down a mirror stuck at `1.0`:
  [diagnose a mirror that reports api-version 1.0](@/ecosystems/pypi/guides/api-version.md)
- Why peryx must not over-advertise: [an honest Simple API version](@/ecosystems/pypi/api-version.md)
