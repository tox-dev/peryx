## Summary

Add full mirror sync and offline serve mode for selected PyPI packages.

Depends on #28 and #30.

## Problem

Velox is a read-through cache. It fetches project pages and artifacts when pip, uv, or a browser asks for them. That
works for live cache use, but it does not cover restricted networks, air-gapped CI, or teams that want to review a
mirror set before clients use it.

Operators also need a clear way to define what they want to mirror. A sync command without a selection model would force
users to choose between mirroring everything and writing scripts around Velox.

## Competitor reference

Bandersnatch supports PyPI mirroring with filters, index-only mode, release-file mirroring, verification, diff files for
offline transfer, alternate download mirrors, and no-fallback download mode.

Reference: https://github.com/pypa/bandersnatch/blob/main/docs/mirror_configuration.md

## Mirror selection model

Velox should support persistent config and CLI overrides.

Example config shape:

```toml
[[index]]
name = "pypi"
route = "pypi"
mirror = "https://pypi.org/simple/"

[index.prefetch]
mode = "selected"
packages = ["requests>=2,<3", "ruff==0.12.*"]
requirements = ["requirements.txt", "constraints.txt"]
include_wheels = true
include_sdists = true
python_tags = ["py3", "cp312"]
abi_tags = ["none", "cp312"]
platform_tags = ["any", "manylinux_2_28_x86_64", "macosx_14_0_arm64"]
metadata_only = false
```

Selection inputs:

- explicit package names and version specifiers
- requirements or constraints files
- policy rules from #28
- artifact filters for package type, Python tag, ABI tag, platform tag, and size
- mode values:
  - `all`: mirror every allowed package from the upstream project list
  - `selected`: mirror only configured package selectors
  - `metadata-only`: mirror Simple pages and metadata without release files

The MVP should not implement dependency resolution. If users want a dependency closure, they should provide a lock file
or generated requirements file that already contains that closure. Velox can parse selectors and mirror matching
projects and files.

## Proposed scope

- Add `peryx mirror plan <repo>` to preview selected projects and files.
- Add `peryx mirror sync <repo>` to prefetch selected Simple API pages, `.metadata` siblings, and artifacts.
- Add `peryx mirror verify <repo>` to check cached metadata and blobs.
- Support selection from config, `--package`, `--requirements`, and policy rules.
- Verify sha256 hashes when upstream provides them.
- Record sync results:
  - start and finish time
  - packages seen
  - files downloaded
  - bytes downloaded
  - skipped files
  - failures with project, filename, URL, and reason
- Add offline serve mode for a repository. In offline mode, Velox must fail closed instead of contacting upstream.
- Add pip and uv integration tests that install from a synced mirror after the fake upstream is shut down.

## Out of scope

- dependency resolution
- static export
- object storage
- cross-node replication
- scanner workflow
- scheduled jobs and job history
- browser UI for sync runs

## Acceptance criteria

- Operators can define a mirror set in config and override it from the CLI.
- `mirror plan` reports what Velox would fetch without writing new cache entries.
- `mirror sync` materializes the selected Simple pages, metadata, and artifacts.
- `mirror verify` detects missing blobs, digest mismatches, and unreadable cached metadata.
- Offline serve mode does not call upstream and returns useful errors for uncached projects or files.
- pip and uv can install selected packages from Velox after upstream shutdown.
