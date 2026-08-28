+++
title = "Policy settings"
description = "PyPI-specific fallback, version, distribution, wheel-tag, release-age, and attestation policy."
weight = 35
+++

PyPI adds package-format rules to the core `[index.policy]` keys.

Policy and access globs match a project after PEP 503 normalization: lowercase the name and replace each run of `.`,
`-`, or `_` with `-`.

| Key                     | Meaning                                                                |
| ----------------------- | ---------------------------------------------------------------------- |
| `fallback_mode`         | Virtual-index selection: `fallback`, `private-first`, or `no-fallback` |
| `allow_versions`        | Accepted PEP 440 version specifier set                                 |
| `allow_package_types`   | Accepted distribution types: `wheel` or `sdist`                        |
| `block_package_types`   | Denied distribution types                                              |
| `allow_wheel_pythons`   | Accepted wheel Python tags                                             |
| `block_wheel_pythons`   | Denied wheel Python tags                                               |
| `allow_wheel_platforms` | Accepted wheel platform tags                                           |
| `block_wheel_platforms` | Denied wheel platform tags                                             |
| `min_release_age_secs`  | Minimum age from the Simple API `upload-time`                          |
| `required_attestations` | Required in-toto predicate types on hosted uploads                     |
| `attestation_mode`      | `enforce` rejects missing attestations; `audit` records and admits     |

```toml
[index.policy]
fallback_mode = "private-first"
allow_versions = ">=1,<3"
allow_package_types = ["wheel"]
block_wheel_platforms = ["win_amd64"]
min_release_age_secs = 604800
required_attestations = ["https://docs.pypi.org/attestations/publish/v1"]
attestation_mode = "enforce"
```

`private-first` serves hosted candidates when both hosted and cached members contain a project. `no-fallback` refuses to
query an immediate cached member. `fallback` preserves filename-level merging. `protected_names` takes precedence over
all three modes.

## Project isolation

The default `fallback` mode keeps the first occurrence of each filename and merges the remaining filenames. If a hosted
layer contains `acme-1.0-py3-none-any.whl` and a cached layer contains `acme-9.0-py3-none-any.whl`, the project page
contains both files. An installer can select `9.0`.

Set `private-first` on the virtual route to exclude cached files whenever a hosted member contains the project:

```toml
[[index]]
ecosystem = "pypi"
name = "pypi"

[[index.upstream]]
name = "primary"
url = "https://pypi.org/simple/"

[[index]]
ecosystem = "pypi"
name = "hosted"
hosted = true

[[index]]
ecosystem = "pypi"
name = "packages"
layers = ["hosted", "pypi"]
write_target = "hosted"

[index.policy]
fallback_mode = "private-first"
```

With this policy, uploading an `acme` file to `hosted` removes all cached `acme` candidates from the `packages` project
page. `no-fallback` excludes the immediate cached member for all projects. `protected_names` denies cached candidates
for matching name globs in each fallback mode. The [team-index tutorial](@/ecosystems/pypi/tutorials/team-index.md) adds
upload credentials and commands for a runnable setup.

Release-age policy hides files with no `upload-time` because their age cannot be established. Attestation policy checks
the bound statements' predicate types; it does not verify signatures, certificates, transparency logs, or publisher
identity. See [upload attestations](@/ecosystems/pypi/reference/uploads.md#attestations) for validation order.

## Retention ordering

PyPI retention plans order versions under [PEP 440](https://peps.python.org/pep-0440/). `2.0` ranks after `2.0rc1`, and
`2.0+local` ranks after `2.0`. Equivalent spellings such as `1.0` and `1.0.0` share one rank, so `keep-latest` counts
releases instead of distribution filenames.

A version that does not parse as PEP 440 follows valid versions and uses its string as a stable tie-break. Within one
version, plans order distribution filenames and digests. The `visibility` selector accepts `active`, `yanked`, and
`hidden` for Python package records.

## Preview decisions

`peryx policy dry-run --index root/pypi --project flask` scans cached Simple pages and hosted file records without
fetching upstreams or changing the served index. It prints tab-separated denial rows with the action, route, normalized
project, filename, version, rule, field, and reason.

## Upload quotas

`max_project_size_bytes` bounds logical bytes for one normalized project. A hosted upload reserves its distribution size
after validation and commits the reservation in the transaction that makes the filename visible. Failed validation,
storage, metadata, project-status, cancellation, and disconnect paths release pending capacity. Re-uploading an
identical filename is idempotent and adds no allocation.

A virtual upload route combines its project limit with the target hosted index. The lower limit applies. Audit mode
applies only when every configured layer enables `quota_audit`; any enforcing layer keeps the combined policy enforcing.
A denial returns `403 Forbidden` with rule `max-project-size`. Lowering a limit below current use does not block reads.

`quota_audit = true` records a violation and admits the upload. Decisions increment `peryx_pypi_quota_admitted_total` or
`peryx_pypi_quota_rejected_total` without a project label. With neither quota mode enabled, uploads perform no
reservation.
