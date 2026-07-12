+++
title = "Wheel .dist-info matching"
description = "How peryx matches a wheel's .dist-info directory to its filename: PEP 503 name normalization and PEP 440 version equality, not a byte-exact string."
weight = 3
+++

Every wheel carries one `*.dist-info` directory holding its `METADATA`, `WHEEL`, and `RECORD`.
[PEP 427](https://packaging.python.org/en/latest/specifications/binary-distribution-format/) names it
`{distribution}-{version}.dist-info`. peryx checks that this directory names the same project and version as the wheel
filename before it reads those files, so a wheel cannot claim to be `requests-2.32.5` while shipping another project's
metadata.

## What peryx compares

peryx derives the project name and version from the filename, then reads the project name and version from the
`.dist-info` directory, and compares the two by value:

- **Project name.** [PEP 503](https://peps.python.org/pep-0503/) normalization on both sides: lowercase, and collapse
  every run of `-`, `_`, or `.` into a single `-`. `Flask`, `flask`, and `FLASK` are one name; `Foo.Bar`, `foo_bar`, and
  `foo--bar` are one name.
- **Version.** [PEP 440](https://peps.python.org/pep-0440/) parsing and equality, not string equality. `1.0` and `1.0.0`
  are the same version, as are `1.0rc1` and `1.0RC1`.

The directory's stem (everything before `.dist-info`) is split into name and version at its **last** hyphen, matching
how the filename splits. peryx does **not** require the directory bytes to equal the normalized filename bytes. An
archive whose directory is spelled the un-normalized way older build tools wrote it is accepted, which is what pip and
[Warehouse](https://pypi.org/) (pypi.org) do. For why, see [un-normalized wheels](@/ecosystems/pypi/dist-info.md).

## Accepted

Each of these wheels is accepted; the filename is on the left, the directory the archive actually contains on the right.

| Wheel filename                    | `.dist-info` directory  | Why it matches                                 |
| --------------------------------- | ----------------------- | ---------------------------------------------- |
| `Flask-0.12-py2.py3-none-any.whl` | `Flask-0.12.dist-info`  | `Flask` and `flask` normalize the same         |
| `foo_bar-1.0-py3-none-any.whl`    | `Foo.Bar-1.0.dist-info` | `Foo.Bar` and `foo_bar` normalize to `foo-bar` |
| `pkg-1.0-py3-none-any.whl`        | `pkg-1.0.0.dist-info`   | `1.0` and `1.0.0` are equal under PEP 440      |

## Rejected

peryx still rejects a directory whose identity genuinely disagrees with the filename, and any archive without exactly
one `.dist-info`. For a wheel filed `Flask-1.0-py3-none-any.whl`, expected `flask-1.0.dist-info`:

| `.dist-info` directory | Error                                                                                  |
| ---------------------- | -------------------------------------------------------------------------------------- |
| `other-1.0.dist-info`  | `.dist-info directory other-1.0.dist-info does not match expected flask-1.0.dist-info` |
| `flask-2.0.dist-info`  | `.dist-info directory flask-2.0.dist-info does not match expected flask-1.0.dist-info` |
| `flask.dist-info`      | `.dist-info directory flask.dist-info does not match expected flask-1.0.dist-info`     |
| none                   | `missing .dist-info directory`                                                         |
| two or more            | `multiple .dist-info directories found: ...`                                           |

A directory with no hyphen in its stem, such as `flask.dist-info`, has no version segment to parse and so cannot match.
A version that does not parse as PEP 440 fails the same way. Every failure is an `invalid wheel:` message and a `400` on
upload.

## The required files

peryx reads `METADATA`, `WHEEL`, and `RECORD` from the directory the archive contains, spelled the way the archive
spells it, not from the normalized name it computed. A missing one of these is a distinct
`missing required <dir>/METADATA` (or `WHEEL`, or `RECORD`) failure.

## In practice

- The full upload checks around this one: [publish packages](@/ecosystems/pypi/guides/publish.md)
- Why the match is normalized rather than byte-exact: [un-normalized wheels](@/ecosystems/pypi/dist-info.md)
- The standards this implements: [standards](@/ecosystems/pypi/reference/standards.md)
