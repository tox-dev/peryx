## Summary

Add a PyPI-specific policy pack for forwarding and dependency-confusion controls.

Depends on #28.

## Problem

Velox overlays can mix local packages with mirrored upstream packages, but operators cannot state fallback behavior as a
policy. A private index needs clear rules for protected package names, public/private name collisions, and blocked
fallback to public upstreams.

Without those rules, a private package name can resolve from PyPI when the local package is absent, renamed, deleted, or
misspelled.

## Competitor reference

GitLab documents PyPI package forwarding as a security concern. Its registry can forward missing package requests to
`pypi.org`, and the docs tell users to disable forwarding or combine `--index-url` with `--no-index` for private
packages.

Reference: https://docs.gitlab.com/user/packages/pypi_repository/

## Proposed scope

- Add policy modes for PyPI indexes:
  - `fallback`: allow upstream fallback for missing local packages
  - `private-first`: prefer local packages and warn on public/private name collisions
  - `no-fallback`: deny upstream lookup when local packages do not satisfy a request
- Add protected-name rules:
  - exact normalized project names
  - prefix or glob-style namespace rules such as `acme-*`
- Detect name collisions after PyPI normalization, including `-`, `_`, and `.` equivalence.
- Return clear blocked-fallback errors that name the project, index, policy mode, and matched rule.
- Show source and policy state in the UI:
  - local
  - upstream
  - shadowed
  - blocked
- Add pip and uv integration tests proving protected names do not fall back to public PyPI candidates.

## Out of scope

- the generic policy evaluation framework tracked in #28
- malware or typosquat detection
- vulnerability and license scanning
- tenant-specific policy inheritance
- global namespace reservation

## Acceptance criteria

- Operators can configure fallback behavior per index.
- Protected names cannot resolve from upstream when policy forbids fallback.
- Local and upstream name collisions produce a visible warning or denial based on policy mode.
- HTML and JSON Simple API responses do not expose blocked upstream files.
- Errors tell users whether a package was missing, blocked by policy, or shadowed by a local package.
- pip and uv tests cover protected names, normalized-name collisions, and no-fallback indexes.
