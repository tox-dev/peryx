+++
title = "Host a verb-named project"
description = "Publish, yank, and delete a project whose normalized name is yank, restore, or promote, and the one addressing rule that matters."
weight = 8
+++

A project whose PEP 503 name is `yank`, `restore`, or `promote` collides with the verbs peryx puts in its mutation URLs.
It uploads and installs like any other package; the one place to get right is the mutation path, where the project name
and the action word are the same. This guide publishes such a project and runs every mutation on it. It assumes a hosted
index reachable at route `root/pypi` with an `upload_token` of `demo-secret`, the shape a default peryx install has once
uploads are enabled.

## The rule

Address the project with its project segment present. peryx reads a trailing `yank`, `restore`, or `promote` as an
action only when a project precedes it, so a lone verb is the project and a suffixed verb is the action:

- `DELETE /root/pypi/yank/` deletes the project `yank`.
- `PUT /root/pypi/yank/yank` yanks the project `yank`.

The trailing slash on the project-level delete is what keeps the name from reading as an action. Names normalize first,
so `Yank` and `YANK` route the same as `yank`.

## Publish

Upload the package as you would any other; the name needs no escaping.

```shell
uv publish --publish-url http://127.0.0.1:4433/root/pypi/ -u __token__ -p demo-secret dist/*
```

## Yank and restore

```shell
# yank one version
curl -X PUT -u __token__:demo-secret http://127.0.0.1:4433/root/pypi/yank/1.0/yank

# yank every file of the project
curl -X PUT -u __token__:demo-secret http://127.0.0.1:4433/root/pypi/yank/yank

# un-yank the project
curl -X DELETE -u __token__:demo-secret http://127.0.0.1:4433/root/pypi/yank/yank
```

Swap `yank` for `restore` to manage a project named `restore`: `PUT /root/pypi/restore/restore` restores every hidden
file of the project `restore`.

## Delete

```shell
# delete one version
curl -X DELETE -u __token__:demo-secret http://127.0.0.1:4433/root/pypi/yank/1.0/

# delete the whole project
curl -X DELETE -u __token__:demo-secret http://127.0.0.1:4433/root/pypi/yank/
```

The last request is the one the old router could not serve: it answered `400` because it read `yank/` as an un-yank of
an empty project. It now returns `200` with the file count and removes the project. Delete needs a `volatile` hosted
layer, the default; an immutable layer answers `403`.

## Promote

`promote` is always versioned and names its source with `from=`:

```shell
curl -X PUT -u __token__:demo-secret 'http://127.0.0.1:4433/root/pypi/promote/1.0/promote?from=staging'
```

A promote without a version answers `400` with `promotion requires a version`, the same as any other project.

## Related

- Why peryx addresses these names: [verb-named projects](@/ecosystems/pypi/reserved-names.md)
- Every path for each verb: [mutation paths for verb-named projects](@/ecosystems/pypi/reference/reserved-names.md)
- The delete walked through end to end: [delete a project named yank](@/ecosystems/pypi/tutorials/reserved-name.md)
