+++
title = "Publish from older tooling"
description = "Upload a wheel whose .dist-info directory casing or version differs from the filename, and tell an equivalent spelling from a genuine mismatch."
weight = 7
+++

You have a wheel built by older tooling, or restored from a backup, whose `.dist-info` directory is not spelled the
normalized way current build backends write it, say `Flask-0.12.dist-info` for a `flask-0.12` filename, or a version
written `1.0.0` where the filename says `1.0`. peryx accepts it, the same way pip and pypi.org do. This guide covers
publishing it and reading a rejection if the directory turns out to name a different release.

## Publish it

Nothing special is required. Upload the wheel to a [hosted route](@/ecosystems/pypi/guides/publish.md) as you would any
other:

```shell
twine upload --repository-url http://127.0.0.1:4433/root/pypi/ \
    -u __token__ -p <secret> dist/Flask-0.12-py2.py3-none-any.whl
```

peryx reads the `.dist-info` directory from the archive, splits its stem into name and version at the last hyphen, and
compares them to the filename by [PEP 503](https://peps.python.org/pep-0503/) name normalization and
[PEP 440](https://peps.python.org/pep-0440/) version equality. An un-normalized but equivalent directory passes:

- `Flask-0.12.dist-info` for `Flask-0.12-py2.py3-none-any.whl`: `Flask` and `flask` normalize the same.
- `Foo.Bar-1.0.dist-info` for `foo_bar-1.0-py3-none-any.whl`: `Foo.Bar` and `foo_bar` both normalize to `foo-bar`.
- `pkg-1.0.0.dist-info` for `pkg-1.0-py3-none-any.whl`: `1.0` and `1.0.0` are equal under PEP 440.

## Check the directory before you upload

If you want to know what peryx will compare, read the directory name out of the archive:

```shell
unzip -l dist/your_pkg-1.0-py3-none-any.whl | grep dist-info
```

Normalize the name in your head (lowercase, and fold every run of `-`, `_`, or `.` to one `-`), then confirm the version
parses to the filename's version. If both agree, the upload will pass regardless of the directory's casing or
separators.

## When it is rejected

A `400` with `invalid wheel: .dist-info directory <dir> does not match expected <expected>` means the directory names a
genuinely different release, not merely a different spelling. peryx builds `<expected>` from the filename, so the
message shows both:

- **Different project.** `other-1.0.dist-info` in a `flask-1.0` wheel. The wheel was mislabeled or repackaged wrong;
  rebuild it or rename the file to match its contents.
- **Different version.** `flask-2.0.dist-info` in a `flask-1.0` wheel. The filename and the metadata disagree on the
  version; fix whichever is wrong.
- **No version segment.** `flask.dist-info`, with no hyphen to split, has no version to compare. The archive is
  malformed; rebuild it.

peryx also rejects an archive with no `.dist-info` directory (`missing .dist-info directory`) or more than one
(`multiple .dist-info directories found: ...`). These are structural faults in the wheel, not spelling differences, so
normalization does not change the outcome. Repacking a wheel by hand is the usual cause; rebuild it with a real backend
instead.

## Related

- The full set of upload checks: [publish packages](@/ecosystems/pypi/guides/publish.md)
- The exact matching rule and its examples: [wheel .dist-info matching](@/ecosystems/pypi/reference/dist-info.md)
- Why the match is normalized: [un-normalized wheels](@/ecosystems/pypi/dist-info.md)
- Walk it end to end: [upload a legacy wheel](@/ecosystems/pypi/tutorials/legacy-wheel.md)
