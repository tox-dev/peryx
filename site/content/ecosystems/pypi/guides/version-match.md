+++
title = "Target a release by version"
description = "Yank, delete, or promote a release addressed by any PEP 440-equivalent version string, even when it differs from the spelling the file was uploaded with."
weight = 8
+++

A release can carry a different version spelling than the one you type. A file uploaded as `1.0` is the same release as
`1.0.0` and `1.0.0.0`, and the version-scoped admin operations match it that way: they compare versions by
[PEP 440](https://peps.python.org/pep-0440/) equality, so you address a release by any equivalent spelling and reach
every file of it. This applies to yank, un-yank, delete, and promote alike.

You need the hosted layer's upload token, the same Basic-auth credential uploads use. The examples assume peryx at
`http://127.0.0.1:4433` with the default virtual route `root/pypi`.

## Find the version you are addressing

You do not have to match the uploaded spelling; any equivalent spelling reaches the file. If you want to see the
spellings on record, read the project page and look at the filenames:

```shell
curl -s -H "Accept: application/vnd.pypi.simple.v1+json" \
    http://127.0.0.1:4433/root/pypi/simple/mypkg/ | python3 -m json.tool | grep filename
```

A file listed as `mypkg-1.0-py3-none-any.whl` is release `1.0`. A request for `1.0`, `1.0.0`, or `1.0.0.0` all reach it.

## Yank or un-yank

```shell
# yank release 1.0 addressed as 1.0.0
curl -X PUT -u __token__:<secret> http://127.0.0.1:4433/root/pypi/mypkg/1.0.0/yank

# un-yank it, addressed with yet another equivalent spelling
curl -X DELETE -u __token__:<secret> http://127.0.0.1:4433/root/pypi/mypkg/1.0.0.0/yank
```

## Delete

```shell
# delete release 1.0 addressed as 1.0.0 (hosted layer must be volatile)
curl -X DELETE -u __token__:<secret> http://127.0.0.1:4433/root/pypi/mypkg/1.0.0/
```

## Promote

```shell
# promote release 1.0 from a staging route, addressed as 1.0.0
curl -X PUT -u __token__:<secret> \
    "http://127.0.0.1:4433/root/pypi/mypkg/1.0.0/promote?from=staging/pypi"
```

## Confirm it landed

Each response is `200` with the number of files affected. A non-zero count means the operation reached the release. Read
the project page back to see the change take effect, for example the `yanked` flag on the file:

```shell
curl -s -H "Accept: application/vnd.pypi.simple.v1+json" \
    http://127.0.0.1:4433/root/pypi/simple/mypkg/ | python3 -m json.tool | grep -A2 mypkg-1.0
```

A `404`, or a count of zero, means nothing matched. Confirm you addressed the right release: an equivalent spelling of a
version that exists will match, but `1.0` never reaches `1.0.1`, and a version carrying a local segment such as
`1.0+build` is a distinct release from `1.0`.

## Related

- Which spellings count as one release, and which do not:
  [version matching for admin operations](@/ecosystems/pypi/reference/version-match.md)
- A worked yank where the spellings differ:
  [yank a release by an equivalent version](@/ecosystems/pypi/tutorials/version-match.md)
- Why the match agrees with the served page: [equivalent version spellings](@/ecosystems/pypi/version-match.md)
