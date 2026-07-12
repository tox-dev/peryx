+++
title = "Un-normalized wheels"
description = "Why peryx matches a wheel's .dist-info directory to its filename by normalized identity instead of exact bytes, and the historical wheels that would fail otherwise."
weight = 5
+++

peryx accepts a wheel whose internal `.dist-info` directory is not spelled the modern, normalized way, as long as it
names the same project and version as the filename. This page explains why the check compares normalized identity rather
than exact bytes, and which real artifacts the byte-exact check used to reject.

## The rule changed under wheels

A wheel's layout is `{name}-{version}.dist-info/`, and the filename is `{name}-{version}-{tags}.whl`. For years the two
`{name}` fields were written however the build tool spelled the project: `Flask-0.12-py2.py3-none-any.whl` shipped a
`Flask-0.12.dist-info` directory, mixed case and all. Only later did the ecosystem settle on
[PEP 503](https://peps.python.org/pep-0503/) normalization, which lowercases the name and folds every run of `-`, `_`,
and `.` to a single `-`, and current build backends write the directory that way. The wheels built before that
convention did not vanish; they are still on PyPI, and installers still install them.

pip and [Warehouse](https://pypi.org/) (pypi.org) never demanded a byte-exact directory. They compare the directory's
project name and version to the filename's after normalizing both, so `Flask-0.12.dist-info` satisfies a `flask-0.12`
filename. peryx now does the same: PEP 503 on the name, [PEP 440](https://peps.python.org/pep-0440/) parsing on the
version. The [reference](@/ecosystems/pypi/reference/dist-info.md) states the exact comparison.

## The failure it prevents

peryx used to build the expected directory name from the filename and require the archive to contain that exact string.
An older wheel whose directory read `Flask-0.12.dist-info` was measured against the computed `flask-0.12.dist-info` and
rejected on upload with `.dist-info directory ... does not match expected ...`, even though the two name the same
release.

That made peryx stricter than the index it stands in front of, and the gap bit where peryx is meant to disappear:

- **Mirroring.** A cached index that pulls a historical wheel from pypi.org, or a migration that re-uploads an
  organization's back catalogue into a hosted index, carries whatever `.dist-info` spelling the original build wrote. A
  file pip installs from pypi.org could not be served through peryx.
- **Re-uploading.** A team moving a private index onto peryx, or restoring from a backup of older builds, hit the same
  wall for artifacts they had shipped for years.

Refusing a wheel that pypi.org accepts breaks the drop-in promise. The index in front of PyPI should take every file
PyPI would. Matching by normalized identity closes that gap while keeping the guarantee that matters. The metadata
inside the wheel belongs to the project and version on the label.

## What stays strict

Normalizing the comparison is not loosening it. A directory whose normalized name or parsed version genuinely differs
from the filename is still rejected, and so is an archive with no `.dist-info` directory or more than one. peryx accepts
a different *spelling* of the right identity; it does not accept the wrong identity. The point is parity with pip and
Warehouse, not leniency past them.

## In practice

- The exact matching rule and its examples: [wheel .dist-info matching](@/ecosystems/pypi/reference/dist-info.md)
- Publish a wheel built by older tooling: [publish from older tooling](@/ecosystems/pypi/guides/legacy-wheel.md)
- Walk an upload of a historical wheel end to end: [upload a legacy wheel](@/ecosystems/pypi/tutorials/legacy-wheel.md)
