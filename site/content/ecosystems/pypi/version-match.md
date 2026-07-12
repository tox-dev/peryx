+++
title = "Equivalent version spellings"
description = "Why yank, delete, and promote match an upload's version by PEP 440 equality instead of exact string, so they agree with the served project page and never miss a file of the release."
weight = 6
+++

A release has more than one spelling. `1.0`, `1.0.0`, and `1.0.0.0` are one version under
[PEP 440](https://peps.python.org/pep-0440/), and every resolver, pip, uv, and pypi.org itself, treats them as one.
peryx serves them as one: a project page filtered to `1.0.0` shows a file whose form version was `1.0`. The
version-scoped admin operations, yank, delete, and promote, have to reach the same file, or they act on a release that
looks different from the one the page shows. This page explains why they match by PEP 440 equality, and the silent
failure a byte-exact match caused.

## Two ways to compare a version

An upload records the version it was published with, whatever spelling the build tool wrote. A team that ships `1.0` and
a team that ships `1.0.0` have published the same release, and their files sit side by side on the project page. When an
operator addresses a release, they type one spelling: `yank 1.0.0`. Two things then have to decide whether a given file
belongs to that request, and they can disagree.

- The **served page** filters by PEP 440 equality. Ask for `1.0.0` and it returns every file of that release, `1.0` and
  `1.0.0.0` included, because that is what a release means to an installer.
- A **byte-exact** match compares the two strings. `1.0.0` does not equal `1.0`, so a file uploaded as `1.0` falls
  outside a request addressed to `1.0.0`.

While the served page used one rule and the mutations used the other, the operator saw one release and the operation
acted on another.

## The failure it prevents

peryx used to compare an upload's version to the requested version byte for byte inside yank, delete, and promote. A
release published as `1.0` was invisible to any request that spelled it another way:

- **Yank did nothing.** `PUT /root/pypi/mypkg/1.0.0/yank` on a file uploaded as `1.0` matched no file, reported zero
  files changed, and left the release live. The operator, reading `1.0.0` off the project page, had every reason to
  think the yank landed, and no sign that it had not.
- **Delete left the file up.** The same mismatch on a delete answered "nothing matched" while the file kept serving.
  Worse, delete falls back to matching on the stored record exactly when the served-page filter finds nothing, so the
  two version notions had to agree or the fallback missed too, in the one place it exists to catch the file.
- **Promote skipped the release.** A promote from a staging route to a release route stepped over a file whose spelling
  did not match, and shipped an incomplete release without saying so.

Each of these fails without a sign. The request succeeds, the count comes back zero, and the file stays as it was. An
operator learns the yank did not take only when a resolver installs the version they thought they had pulled.

## Why equality is the right rule

The operations route their version comparison through the same PEP 440 equality the served page uses, with a fall back
to byte comparison when a version does not parse. Addressing any spelling of a release now reaches every file of that
release, and the operation acts on the set of files the page shows for that version. The two sides of peryx, the page a
client reads and the mutation an operator runs, share one definition of what a release is.

## What stays strict

Equality is one release, not a loose match. A request for `1.0` reaches `1.0.0` but never `1.0.1` or `1.1`; those are
different releases and stay untouched. The [local segment](https://peps.python.org/pep-0440/#local-version-identifiers)
counts: `1.0+build` and `1.0` are distinct versions and do not match. And a version that is not valid PEP 440 is
compared by its exact spelling, so a non-standard tag matches only itself. peryx reaches every spelling of the right
release; it does not reach the wrong one.

## In practice

- The exact matching rule and its examples:
  [version matching for admin operations](@/ecosystems/pypi/reference/version-match.md)
- Address a release by any equivalent spelling: [target a release by version](@/ecosystems/pypi/guides/version-match.md)
- Walk a mismatched yank end to end:
  [yank a release by an equivalent version](@/ecosystems/pypi/tutorials/version-match.md)
