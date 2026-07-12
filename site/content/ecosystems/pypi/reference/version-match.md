+++
title = "Version matching for admin operations"
description = "How yank, un-yank, delete, and promote match an upload's version: PEP 440 equality of the release, not exact string, with a byte fallback for versions that do not parse."
weight = 4
+++

The version-scoped admin operations address a release by version: yank, un-yank, delete, and promote. Each reads the
version recorded on every upload of the project and acts on the files whose version matches the one in the request. The
match is [PEP 440](https://peps.python.org/pep-0440/) equality of the release, not a byte-exact comparison of the two
strings, so a request addressed to `1.0.0` reaches a file uploaded with form version `1.0`.

## The rule

Two versions match when either holds:

- their strings are byte-identical, or
- both parse as PEP 440 versions and those parsed versions are equal.

When either string fails to parse as a PEP 440 version, only the byte-identical case remains: the comparison falls back
to exact string equality. This is the same equality the served project page applies when it decides which files a
version filter shows, so an operation and the page it acts on agree on what one release is.

## What counts as equal

PEP 440 equality normalizes the release segment, so trailing-zero spellings of the same release are equal, while a
different release, or a version carrying a distinct
[local segment](https://peps.python.org/pep-0440/#local-version-identifiers), is not.

| Requested   | Recorded on upload | Match | Why                                             |
| ----------- | ------------------ | ----- | ----------------------------------------------- |
| `1.0.0`     | `1.0`              | yes   | same release, `1.0` == `1.0.0`                  |
| `1.0.0.0`   | `1.0`              | yes   | same release, trailing zeros normalize          |
| `1.0.0.0`   | `1.0.0`            | yes   | same release                                    |
| `1.0.0`     | `1.0.1`            | no    | different release                               |
| `1.0+build` | `1.0.0+build`      | yes   | same release and same local segment             |
| `1.0+build` | `1.0`              | no    | local segment present on one side only          |
| `1.0.0`     | `nightly`          | no    | `nightly` does not parse; byte comparison fails |
| `nightly`   | `nightly`          | yes   | neither parses; byte-identical                  |

## The record fallback

Matching reads the version stored on each upload record, the form value captured when the file was published, not a
value re-derived from the filename. When that stored string is not a parseable PEP 440 version, or the requested version
is not, the comparison is byte-exact: an unparseable recorded version matches only a request that spells it the same
way. Delete relies on this. When the served-page filter matches nothing, delete falls back to matching on the stored
record, and the two notions of equality have to agree or the fallback misses the file it should remove.

## Scope

The rule governs every version-scoped form of these endpoints:

- `PUT /{route}/{project}/{version}/yank` and its `DELETE` un-yank
- `DELETE /{route}/{project}/{version}/`
- `PUT /{route}/{project}/{version}/promote?from=...`

The project-wide forms that carry no version, such as `PUT /{route}/{project}/yank`, act on every file of the project
and never compare versions.

## What it does not do

The match is equality of one release, not a range or a prefix. A request for `1.0` does not reach `1.0.1` or `1.1`. It
does not ignore the local segment: `1.0+build` and `1.0` are distinct releases. And it never rewrites a stored version;
the record keeps the spelling it was uploaded with, and matching is decided per request.

## Related

- Address a release by any equivalent spelling: [target a release by version](@/ecosystems/pypi/guides/version-match.md)
- Watch a mismatched spelling take effect:
  [yank a release by an equivalent version](@/ecosystems/pypi/tutorials/version-match.md)
- Why the match has to agree with the served page: [equivalent version spellings](@/ecosystems/pypi/version-match.md)
