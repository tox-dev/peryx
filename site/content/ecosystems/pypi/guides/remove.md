+++
title = "Yank and delete packages"
description = "Yank an uploaded release per PEP 592 or delete it outright, address a release by any equivalent version spelling, and manage a project named after a mutation verb."
weight = 6
aliases = [
    "/ecosystems/pypi/guides/version-match/",
    "/ecosystems/pypi/guides/reserved-name/",
]
+++

Both operations take the same Basic-auth token as uploads, and both work on upstream files too: a cached index is
read-only, so peryx records the change as a reversible override on the virtual index's hosted layer instead of touching
the cached index. Removing an uploaded file that shadowed an upstream one makes the upstream version visible again.

## Yank (reversible)

Yanking marks the file per [PEP 592](https://peps.python.org/pep-0592/): resolvers skip it, but an installation pinned
to the exact version can still fetch it. Audit trails and pinned builds survive.

```shell
# yank one version
curl -X PUT -u __token__:<secret> http://127.0.0.1:4433/root/pypi/mypkg/1.2.0/yank

# yank every file of the project
curl -X PUT -u __token__:<secret> http://127.0.0.1:4433/root/pypi/mypkg/yank

# un-yank
curl -X DELETE -u __token__:<secret> http://127.0.0.1:4433/root/pypi/mypkg/1.2.0/yank
```

## Delete

Deleting uploaded files removes their records outright and requires the hosted layer to be `volatile` (the default); set
`volatile = false` on release indexes you want immutable, and peryx answers `403` instead. Deleting files that come from
a cached index hides them from the virtual index reversibly; `restore` undoes it.

```shell
# delete one version
curl -X DELETE -u __token__:<secret> http://127.0.0.1:4433/root/pypi/mypkg/1.2.0/

# delete the whole project
curl -X DELETE -u __token__:<secret> http://127.0.0.1:4433/root/pypi/mypkg/
```

## Restore hidden upstream files

```shell
curl -X PUT -u __token__:<secret> http://127.0.0.1:4433/root/pypi/mypkg/1.2.0/restore  # one version
curl -X PUT -u __token__:<secret> http://127.0.0.1:4433/root/pypi/mypkg/restore        # whole project
```

The content-addressed blob stays on disk after a delete; another index or a re-upload with the same digest reuses it.
Responses are `200` with the number of files affected, or `404` when nothing matched. The project page's "Manage
uploads" panel in the [web UI](@/core/web-ui.md) drives these same endpoints.

## Target a release by an equivalent version

A release can carry a different version spelling than the one you type. A file uploaded as `1.0` is the same release as
`1.0.0` and `1.0.0.0`, and the version-scoped operations match it that way: they compare versions by
[PEP 440](https://peps.python.org/pep-0440/) equality, so you address a release by any equivalent spelling and reach
every file of it. This applies to yank, un-yank, delete, and promote alike.

### Find the version you are addressing

You do not have to match the uploaded spelling; any equivalent spelling reaches the file. If you want to see the
spellings on record, read the project page and look at the filenames:

```shell
curl -s -H "Accept: application/vnd.pypi.simple.v1+json" \
    http://127.0.0.1:4433/root/pypi/simple/mypkg/ | python3 -m json.tool | grep filename
```

A file listed as `mypkg-1.0-py3-none-any.whl` is release `1.0`. A request for `1.0`, `1.0.0`, or `1.0.0.0` all reach it.

### Address it by an equivalent spelling

```shell
# yank release 1.0 addressed as 1.0.0
curl -X PUT -u __token__:<secret> http://127.0.0.1:4433/root/pypi/mypkg/1.0.0/yank

# un-yank it, addressed with yet another equivalent spelling
curl -X DELETE -u __token__:<secret> http://127.0.0.1:4433/root/pypi/mypkg/1.0.0.0/yank

# delete release 1.0 addressed as 1.0.0 (hosted layer must be volatile)
curl -X DELETE -u __token__:<secret> http://127.0.0.1:4433/root/pypi/mypkg/1.0.0/

# promote release 1.0 from a staging route, addressed as 1.0.0
curl -X PUT -u __token__:<secret> \
    "http://127.0.0.1:4433/root/pypi/mypkg/1.0.0/promote?from=staging/pypi"
```

### Confirm it landed

Each response is `200` with the number of files affected. A non-zero count means the operation reached the release. Read
the project page back to see the change take effect, for example the `yanked` flag on the file:

```shell
curl -s -H "Accept: application/vnd.pypi.simple.v1+json" \
    http://127.0.0.1:4433/root/pypi/simple/mypkg/ | python3 -m json.tool | grep -A2 mypkg-1.0
```

A `404`, or a count of zero, means nothing matched. Confirm you addressed the right release: an equivalent spelling of a
version that exists will match, but `1.0` never reaches `1.0.1`, and a version carrying a local segment such as
`1.0+build` is a distinct release from `1.0`.

## Host a verb-named project

A project whose normalized name is `yank`, `restore`, or `promote` collides with the verbs peryx puts in its mutation
URLs. It uploads and installs like any other package; the one place to get right is the mutation path, where the project
name and the action word are the same. The examples below use a project whose normalized name is `yank`; names
normalize first, so `Yank` and `YANK` route the same as `yank`.

### The rule

Address the project with its project segment present. peryx reads a trailing `yank`, `restore`, or `promote` as an
action only when a project precedes it, so a lone verb is the project and a suffixed verb is the action:

- `DELETE /root/pypi/yank/` deletes the project `yank`.
- `PUT /root/pypi/yank/yank` yanks the project `yank`.

The trailing slash on the project-level delete is what keeps the name from reading as an action.

### Publish, yank, restore, and delete it

Upload the package as you would any other; the name needs no escaping.

```shell
uv publish --publish-url http://127.0.0.1:4433/root/pypi/ -u __token__ -p <secret> dist/*
```

```shell
# yank one version
curl -X PUT -u __token__:<secret> http://127.0.0.1:4433/root/pypi/yank/1.0/yank

# yank every file of the project
curl -X PUT -u __token__:<secret> http://127.0.0.1:4433/root/pypi/yank/yank

# un-yank the project
curl -X DELETE -u __token__:<secret> http://127.0.0.1:4433/root/pypi/yank/yank

# delete one version
curl -X DELETE -u __token__:<secret> http://127.0.0.1:4433/root/pypi/yank/1.0/

# delete the whole project
curl -X DELETE -u __token__:<secret> http://127.0.0.1:4433/root/pypi/yank/
```

Swap `yank` for `restore` to manage a project named `restore`: `PUT /root/pypi/restore/restore` restores every hidden
file of the project `restore`. The last delete is the one the old router could not serve: it answered `400` because it
read `yank/` as an un-yank of an empty project. It now returns `200` with the file count and removes the project. Delete
needs a `volatile` hosted layer, the default; an immutable layer answers `403`.

`promote` is always versioned and names its source with `from=`:

```shell
curl -X PUT -u __token__:<secret> 'http://127.0.0.1:4433/root/pypi/promote/1.0/promote?from=staging'
```

A promote without a version answers `400` with `promotion requires a version`, the same as any other project.

## Related

- Yank vs delete vs hide, and why all three exist: [the index model](@/core/indexes.md)
- The same actions from the browser: [the web UI](@/core/web-ui.md)
- The exact matching rule, and every path for a verb-named project:
  [version matching for admin operations](@/ecosystems/pypi/reference/uploads.md#version-matching-for-admin-operations)
  and [mutation paths for verb-named projects](@/ecosystems/pypi/reference/uploads.md#mutation-paths-for-verb-named-projects)
- Why the match agrees with the served page, and why peryx addresses these names:
  [equivalent version spellings](@/ecosystems/pypi/uploads.md#equivalent-version-spellings) and
  [verb-named projects](@/ecosystems/pypi/uploads.md#verb-named-projects)
- Walk an equivalent-version yank and a verb-named delete end to end:
  [publish and manage a release](@/ecosystems/pypi/tutorials/publish-and-manage.md)
