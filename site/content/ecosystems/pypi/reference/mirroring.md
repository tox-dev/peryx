+++
title = "Mirroring"
description = "Select, sync, and verify PyPI projects and artifacts."
weight = 7
+++

`peryx mirror` reads package selection from `[index.prefetch]` on a cached PyPI index. Command-line options add
selectors or override filters for one run.

```toml
[index.prefetch]
mode = "selected"
packages = ["requests>=2,<3"]
requirements = ["requirements.txt"]
include_wheels = true
include_sdists = true
python_tags = ["py3", "cp312"]
abi_tags = ["none", "abi3"]
platform_tags = ["any", "manylinux_2_28_x86_64"]
max_file_size_bytes = 524288000
metadata_only = false
```

| Key                   | Values                             | Default    |
| --------------------- | ---------------------------------- | ---------- |
| `mode`                | `selected`, `all`, `metadata-only` | `selected` |
| `packages`            | Package selectors                  | `[]`       |
| `requirements`        | Requirements or constraints files  | `[]`       |
| `include_wheels`      | Boolean                            | `true`     |
| `include_sdists`      | Boolean                            | `true`     |
| `python_tags`         | Wheel Python tags                  | `[]`       |
| `abi_tags`            | Wheel ABI tags                     | `[]`       |
| `platform_tags`       | Wheel platform tags                | `[]`       |
| `max_file_size_bytes` | Positive integer                   | Unbounded  |
| `metadata_only`       | Boolean                            | `false`    |

`mode = "all"` reads the upstream Simple project list before visiting project pages: `sync` publishes that list as the
index's catalog, while `plan` parses it for the preview and keeps nothing. `mode = "metadata-only"` implies
`metadata_only = true`. Artifact filters run after a project page is fetched.

```shell
peryx mirror plan root/pypi --option 'packages=["requests>=2,<3"]'
peryx mirror sync root/pypi --option 'requirements=["requirements.txt"]'
peryx mirror sync pypi --option 'mode="all"' --option 'python_tags=["py3"]' --option 'abi_tags=["none"]' --option 'platform_tags=["any"]'
peryx mirror verify pypi --option 'mode="all"'
```

Operators can narrow one run with `--option 'no_wheels=true'`, `--option 'no_sdists=true'`,
`--option 'metadata_only=true'`, or `--option 'max_file_size_bytes=524288000'`. They use `packages` or `requirements` to
add selectors to configured lists, and each `*_tags` override adds wheel tags. `mode` and `max_file_size_bytes` replace
their configured values. Setting the three boolean examples to `true` narrows the selection.

`peryx mirror plan` and `peryx mirror sync` overlap project pages and artifact transfers rather than waiting for each
one. The ceiling is the index's `upstream_concurrency`, or eight when the index is uncapped, and project pages and
artifact transfers spend it together, so one project with many artifacts costs the same budget as many projects with
one. Each task carries its own report rows and counters, so a stalled transfer holds back only the rows behind it and
the report still reads in selection order. `peryx mirror verify` reaches no upstream and rehashes up to eight cached
blobs at once.
