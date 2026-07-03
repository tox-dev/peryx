## Summary

Add a repository policy engine that decides whether Velox may upload, mirror, cache, or serve a project release file.

## Problem

Velox currently accepts upstream artifacts and local uploads based on protocol shape and local index configuration. It
does not give operators a single policy layer for package allow/block rules, version ranges, wheel tags, package type,
metadata, file size, or upstream origin.

That leaves common registry controls outside the product:

- run a smaller approved mirror
- block known-bad or unwanted packages before clients can resolve them
- reject upload or mirror actions with a clear policy reason
- preview the effect of a policy before enabling it

## Competitor reference

Bandersnatch supports mirror filters for package allow/block lists, PEP 440 version specifiers, requirements files,
project and release metadata, regex matches, prerelease filtering, platform and Python wheel filtering, latest-N release
selection, project size thresholds, and version-count thresholds.

Reference: https://github.com/pypa/bandersnatch/blob/main/docs/filtering_configuration.md

## Proposed scope

- Add policy configuration for:
  - project allow/block lists using normalized PyPI names
  - version specifiers
  - package types such as wheel and sdist
  - wheel platform/Python filters
  - per-file and per-project size limits
- Apply policy to:
  - local upload acceptance
  - upstream Simple page ingestion
  - upstream artifact downloads
  - admin preview or dry-run checks
- Return structured denial errors that include:
  - action, such as upload, mirror, cache, or serve
  - project, filename, and version when known
  - rule name and matched field
  - short operator-facing reason
- Add dry-run output that reports which projects or files would be blocked without changing the served index.
- Add pip and uv integration tests proving blocked files do not appear in Simple API responses or install candidates.

## Out of scope

- vulnerability scanning
- license scanning
- quota accounting and retention cleanup
- tenant policy inheritance
- browser-based policy editing

Those should depend on this issue if we accept them later.

## Acceptance criteria

- Operators can configure allow/block and size/tag policies without writing code.
- Velox enforces policy consistently across upload, mirror ingestion, artifact fetch, and Simple API output.
- Policy denials return actionable errors and do not leak blocked artifacts through HTML or JSON Simple API responses.
- Dry-run mode explains what would change before enforcement.
- Tests cover upload rejection, upstream filtering, wheel tag filtering, size filtering, dry-run output, and pip/uv
  client behavior.
