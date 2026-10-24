+++
title = "Release"
description = "Plan, build, publish, and verify a cargo-dist release."
weight = 30
+++

Cargo-dist plans the release from `dist-workspace.toml`. The repository customizes `.github/workflows/release.yml` to
check the tagged source, scope credentials, render notes, and publish the Python artifacts. Change `dist-workspace.toml`
for distribution targets and installers; change the workflow for job order or authentication. Run `just release-plan`
after either change.

`cargo-dist-version` selects the configuration and generator contract. The `aqua:axodotdev/cargo-dist` entry in
`mise.toml` installs the CLI, and `mise.lock` records its resolved release with a checksum. Renovate updates the
configuration version; weekly mise lock maintenance updates the CLI resolution.

Pull requests run the cargo-dist planning path. The plan covers five archive targets, shell and PowerShell installers,
checksums, CycloneDX manifests, cargo-auditable metadata, GitHub attestations, and the package publication job.

## Validate a release change

Run the plan and the complete local gate from the repository root:

```shell
just release-plan
just all
```

`just all` runs the lint lanes, native and frontend coverage, and the documentation build.

Build the Python artifacts from the checkout when changing Python packaging:

```shell
just package-sdist .tox/dist
just package-wheel
```

Inspect the cargo-dist plan for the expected targets, installers, checksums, attestations, and custom publish job.

## Document a change

Each pull request needs a change file unless a maintainer applies the `skip news` label. Run `knope document-change`,
pick the change type, and write the summary; knope saves it under `.changeset/`. Use `minor` for Added, `change` for
Changes, `patch` for Fixed, and `major` for Breaking changes. Documentation, Security, Performance, Deprecated, Removed,
and Packaging have matching custom types in `knope.toml`. A Python support addition or removal belongs under Added.
Knope's conventional commit parser stays off because it rejects subjects that start with an emoji, as squash commits
here do. CI validates every supplied change file and requires one on human-authored pull requests without the
`skip news` label. Bot pull requests need one only if their changes warrant news; their supplied files still receive
validation.

Pull request documentation builds show current change files under Draft on the changelog page. The release workflow
moves those entries into a versioned section when it prepares the release.

## Publish a release

peryx uses calendar versions of the form `YYYY.MDD.N`: the UTC year, then the month and zero-padded day, then a counter
for releases on the same day. The first release on 24 September 2026 is `2026.924.0`, the next that day `2026.924.1`,
and the first on 5 November `2026.1105.0`. Every part stays a plain number, so the version is valid for Cargo and PyPI,
and versions sort in release order.

Start the `Prepare release` workflow from the Actions tab, or from a shell:

```shell
gh workflow run prepare-release.yml --repo tox-dev/peryx
```

The workflow computes the version from the date and the existing tags. It stops if `.changeset/` has no change files.
Release-creator links each merged change file to its squash pull request and human authors before Knope collects it.
Knope updates the manifests, `Cargo.lock`, `site/static/openapi.json`, and `CHANGELOG.md`; release-creator then adds
stable anchors to each version and category heading. Prek checks the assembled patch without release credentials. A
second job checks those changes, commits them to `main`, and tags `v<version>`. One atomic push lands both.

The tag starts `release.yml`. Its keyless plan runs prek again on the exact tagged source and rejects a version without
a changelog section. The workflow builds archives, stages a draft GitHub Release with the tagged notes and assets,
publishes and verifies the PyPI package, then makes the GitHub Release public. The notes keep cargo-dist's installer,
download, and verification instructions and link to PyPI when that version is published there.

The global build generates the planned CycloneDX manifest and checks every planned file before upload. A read-only host
job checks the plan, source, tag, file set, and archive digests. Separate jobs attest the sealed files and stage the
GitHub draft; the staging job has no checkout or build tools.

The wheel and sdist jobs install Maturin from the `build` selection in `pylock.toml`. Before PyPI publishing, a job
without publishing credentials requires the wheel for each platform and the sdist, checks their metadata and wheel
contents, and records their hashes. It compares that set with PyPI and passes only missing filenames to the PyPI job.
That job copies the matching validated files, attests them, and uploads them. A retry can skip files already uploaded;
the following keyless job checks all nine filenames and hashes on PyPI before the GitHub draft is published.

The push authenticates as the `peryx-release` GitHub App, the one actor besides repository admins that the `main`
ruleset lets bypass required checks and the tag ruleset lets create `v*` tags. The `release-auth` environment holds its
client ID in the `RELEASE_APP_CLIENT_ID` variable and its private key in the `RELEASE_APP_PRIVATE_KEY` secret; each run
mints a token that expires with the job. A tag pushed with `GITHUB_TOKEN` would not start `release.yml`. The `release`
environment gates the jobs that generate contributor credits and stage and publish the GitHub release; the `pypi`
environment gates trusted publishing. Both environments accept only `v*` tags and require no reviewer.

## Verify a release

1. Before starting the workflow, run `just release-plan` and `just all` on `main`.
1. If the release changes a capability compared in a migration page, verify the Peryx mapping against the shipped
   configuration, CLI, routes, and tests, then refresh each external claim from a current primary source.
1. Wait for all build, host, and custom publication jobs to pass.
1. Download each archive and verify its checksum and GitHub attestation.
1. Inspect the CycloneDX manifest and cargo-auditable metadata from one executable per platform family.
1. Test the shell installer, PowerShell installer, and affected Python package on their target platforms.

A checksum detects changed bytes. The attestation ties those bytes to this repository, workflow, and source revision;
verify both.

Owner-specific release instructions remain with the owner documentation.
