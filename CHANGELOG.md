# Changelog

Each release lists user-facing changes merged since the previous one. Versions follow the calendar as `YYYY.MDD.N`: the
year, the month and zero-padded day, and a counter for several releases on one day.

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
