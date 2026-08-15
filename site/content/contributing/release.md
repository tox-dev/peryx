+++
title = "Release"
description = "Plan, build, publish, and verify a cargo-dist release."
weight = 30
+++

Cargo-dist generates `.github/workflows/release.yml` from `dist-workspace.toml`. Do not edit the generated workflow by
hand. Change the distribution configuration and regenerate it through cargo-dist.

Pull requests run the cargo-dist planning path. The plan covers five archive targets, shell and PowerShell installers,
checksums, CycloneDX manifests, cargo-auditable metadata, GitHub attestations, and the package publication job.

## Validate a release change

Run the plan and the complete local gate from the repository root:

```shell
just release-plan
just all
```

`just all` runs all non-system crate contracts through `just coverage`, then merges system and frontend reports. It also
runs source, documentation, automation, dependency, API, feature, package, release-plan, site, and CI-safe hook checks.
Do not rerun crate contracts as a separate release step.

Use Compose when the host cannot run the Linux coverage or system dependencies:

```shell
just linux-system all
```

Build the Python artifacts from the checkout when changing Python packaging:

```shell
just package-sdist .tox/dist
just package-wheel
```

Inspect the cargo-dist plan for the expected targets, installers, checksums, attestations, and custom publish job.

## Publish and verify

1. Run `just release-plan` and `just all` on the commit to tag.
1. Confirm that the version, lockfile, release notes, and generated plan refer to that commit.
1. Create the release tag expected by cargo-dist.
1. Wait for all build, host, and custom publication jobs to pass.
1. Download each archive and verify its checksum and GitHub attestation.
1. Inspect the CycloneDX manifest and cargo-auditable metadata from one executable per platform family.
1. Test the shell installer, PowerShell installer, and affected Python package on their target platforms.

A checksum detects changed bytes. The attestation ties those bytes to this repository, workflow, and source revision;
verify both.

Owner-specific release instructions remain with the owner documentation.
