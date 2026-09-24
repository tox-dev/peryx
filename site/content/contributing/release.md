+++
title = "Release"
description = "Plan, build, publish, and verify a cargo-dist release."
weight = 30
+++

Cargo-dist generates `.github/workflows/release.yml` from `dist-workspace.toml`. Do not edit the generated workflow by
hand. Change the distribution configuration and regenerate it through cargo-dist.

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

A pull request that changes what users see adds a change file. Run `knope document-change`, pick the change type, and
write the summary; knope saves it under `.changeset/`. The change type picks the changelog section: `major` lists under
breaking changes, `minor` under features, and `patch` under fixes. Release notes come only from these files: knope's
conventional commit parser rejects subjects that start with an emoji, as squash commits here do, so it would pick up
only the few older subjects without one.

## Publish a release

peryx uses calendar versions of the form `YYYY.MDD.N`: the UTC year, then the month and zero-padded day, then a counter
for releases on the same day. The first release on 24 September 2026 is `2026.924.0`, the next that day `2026.924.1`,
and the first on 5 November `2026.1105.0`. Every part stays a plain number, so the version is valid for Cargo and PyPI,
and versions sort in release order.

Start the `Prepare release` workflow from the Actions tab, or from a shell:

```shell
gh workflow run prepare-release.yml --repo tox-dev/peryx
```

The workflow stops when `.changeset/` holds no change files. Otherwise it computes the version from the date and the
existing tags and runs `knope prepare-release`, which writes the version into every manifest, `Cargo.lock`, and
`site/static/openapi.json`, moves the change files into `CHANGELOG.md`, and commits the result to `main` with a
`v<version>` tag. One atomic push lands both, and the tag starts `release.yml`, which builds the archives, creates the
GitHub release with the `CHANGELOG.md` section as its notes, and publishes the PyPI package.

The push authenticates as the `peryx-release` GitHub App, the one actor besides repository admins that the `main`
ruleset lets bypass required checks and the tag ruleset lets create `v*` tags. The `release-auth` environment holds its
client ID in the `RELEASE_APP_CLIENT_ID` variable, its bot user ID in `RELEASE_APP_USER_ID`, and its private key in the
`RELEASE_APP_PRIVATE_KEY` secret; each run mints a token that expires with the job. A tag pushed with `GITHUB_TOKEN`
would not start `release.yml`. The PyPI job authenticates through a trusted publisher for `release.yml` in the `pypi`
environment.

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
