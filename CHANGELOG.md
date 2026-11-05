# Changelog

Each release lists user-facing changes merged since the previous one. Versions follow the calendar as `YYYY.MDD.N`: the
year, the month and zero-padded day, and a counter for several releases on one day.

<a name="release-2026-1105-0"></a>

## 2026.1105.0 (2026-11-05)

<a name="release-2026-1104-0"></a>

## 2026.1104.0 (2026-11-04)

<a name="release-2026-1104-0-fixed"></a>

### Fixed

#### Commit sidecar rows off the async worker ([#2394](https://github.com/tox-dev/peryx/pull/2394)) by [@amitmi704](https://github.com/amitmi704) and [@gaborbernat](https://github.com/gaborbernat)

A burst of cold wheel metadata sidecars no longer holds the request workers while the metadata store syncs to disk, so
`/+status` keeps answering during the burst.

<a name="release-2026-1027-1"></a>

## 2026.1027.1 (2026-10-27)

<a name="release-2026-1027-1-changes"></a>

### Changes

#### Write policy decisions from a background batch writer ([#2378](https://github.com/tox-dev/peryx/pull/2378)) by [@amitmi704](https://github.com/amitmi704) and [@gaborbernat](https://github.com/gaborbernat)

A project page no longer blocks runtime workers on one metadata write transaction per listed file. A single writer
thread now commits queued policy decisions together, in arrival order, and reads of decisions wait for earlier records.
The writer opens each table once per batch instead of once per decision, and it prunes history down to the retention
bound with one range deletion instead of one removal per row.

<a name="release-2026-1027-0"></a>

## 2026.1027.0 (2026-10-27)

<a name="release-2026-1027-0-changes"></a>

### Changes

- Raise the aws-config, aws-sdk-s3, aws-smithy-http-client, and atomicwrites floors and hold unicode-normalization on
  Unicode 17 data ([#2381](https://github.com/tox-dev/peryx/pull/2381)) by
  [@gaborbernat](https://github.com/gaborbernat)
- Update tantivy, tokio, zstd, and the rustls certificate crates past published security advisories
  ([#2379](https://github.com/tox-dev/peryx/pull/2379)) by [@gaborbernat](https://github.com/gaborbernat)

<a name="release-2026-1027-0-fixed"></a>

### Fixed

- Fix the icon and contributor links in the PyPI description ([#2377](https://github.com/tox-dev/peryx/pull/2377)) by
  [@gaborbernat](https://github.com/gaborbernat)

<a name="release-2026-1010-0"></a>

## 2026.1010.0 (2026-10-10)

<a name="release-2026-1010-0-added"></a>

### Added

- Require Python 3.11 or newer and use Python 3.15 by default ([#2373](https://github.com/tox-dev/peryx/pull/2373)) by
  [@gaborbernat](https://github.com/gaborbernat).

<a name="release-2026-927-0"></a>

## 2026.927.0 (2026-09-27)

<a name="release-2026-927-0-added"></a>

### Added

- Create an administrator account on first start ([#2358](https://github.com/tox-dev/peryx/pull/2358)) by
  [@gaborbernat](https://github.com/gaborbernat).
- Sign in with a local password ([#2359](https://github.com/tox-dev/peryx/pull/2359)) by
  [@gaborbernat](https://github.com/gaborbernat).
- Change a local password from the web interface ([#2362](https://github.com/tox-dev/peryx/pull/2362)) by
  [@gaborbernat](https://github.com/gaborbernat).

<a name="release-2026-927-0-fixed"></a>

### Fixed

- Hide the login link when it offers no sign-in method ([#2357](https://github.com/tox-dev/peryx/pull/2357)) by
  [@gaborbernat](https://github.com/gaborbernat).

<a name="release-2026-927-0-security"></a>

### Security

- End active sessions after a password change ([#2363](https://github.com/tox-dev/peryx/pull/2363)) by
  [@gaborbernat](https://github.com/gaborbernat).
- End active sessions after any password write ([#2364](https://github.com/tox-dev/peryx/pull/2364)) by
  [@gaborbernat](https://github.com/gaborbernat).

<a name="release-2026-925-0"></a>

## 2026.925.0 (2026-09-25)

<a name="release-2026-925-0-added"></a>

### Added

- First public release: peryx caches and hosts PyPI packages and OCI images as one executable. Install it with the shell
  or PowerShell installer from the GitHub Release, or with `pip install peryx`.
