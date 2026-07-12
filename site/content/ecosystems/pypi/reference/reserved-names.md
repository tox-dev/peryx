+++
title = "Mutation paths for verb-named projects"
description = "How peryx routes yank, restore, delete, and promote when the project is itself named yank, restore, or promote, and the project-level delete of a project named yank this fixed."
weight = 4
+++

peryx names its mutation actions in the URL. A `PUT` yanks, restores, or promotes; a `DELETE` deletes or un-yanks. The
action is the last path segment: `PUT /{route}/{project}/yank`, `DELETE /{route}/{project}/yank` (un-yank),
`PUT /{route}/{project}/{version}/restore`. `yank`, `restore`, and `promote` are also legal
[PEP 503](https://peps.python.org/pep-0503/) project names, so a project can be named after the verb that acts on it.

## The grammar

peryx peels a trailing action segment only when a project segment precedes it: the text left after removing the verb
must end in `/`, so the request names a project before it names an action. A path that is nothing but the verb is not an
action, it is the project. Names are compared after PEP 503 normalization, so `Yank`, `YANK`, and `yank` are the same
project and collide the same way.

The table uses route `root/pypi` and a project whose normalized name is `yank`.

| Request                          | Meaning                        |
| -------------------------------- | ------------------------------ |
| `DELETE /root/pypi/yank/`        | delete the project `yank`      |
| `DELETE /root/pypi/yank/1.0/`    | delete version `1.0` of `yank` |
| `PUT /root/pypi/yank/yank`       | yank every file of `yank`      |
| `PUT /root/pypi/yank/1.0/yank`   | yank version `1.0` of `yank`   |
| `DELETE /root/pypi/yank/yank`    | un-yank the project `yank`     |
| `PUT /root/pypi/restore/restore` | restore the project `restore`  |

`promote` is always versioned and takes `from={source route}`, so its verb-named form is
`PUT /root/pypi/promote/1.0/promote?from=staging` to promote version `1.0` of the project `promote`. A promote without a
version answers `400` with `promotion requires a version`, verb-named or not.

## What changed

peryx used to strip the verb even when it was the whole path, reading the request as the action on an empty project.
`DELETE /root/pypi/yank/`, a delete of the project `yank`, parsed as an un-yank of a project with no name and failed
validation with `400 Bad Request`. The project named `yank` had no working project-level delete: its own name shadowed
the action. The versioned delete `DELETE /root/pypi/yank/1.0/` and the project-level yank `PUT /root/pypi/yank/yank`
already worked, because each puts a project segment before the trailing token.

The scope was narrow. `DELETE` peels only `yank`, so `yank` was the one project name whose project-level delete broke;
`restore` and `promote` never collided on `DELETE`. The fix drops the whole-path case from the grammar for every verb,
so a project named after any mutation verb stays addressable on both methods.

## Not affected

Uploading a project named `yank`, `restore`, or `promote` was never blocked; the collision lived only in the mutation
router, and the upload path parses the name straight. Every request above takes the same upload token as any other
mutation, and a `200` carries the number of files affected, a `404` means nothing matched.

## Related

- Why peryx addresses these names at all: [verb-named projects](@/ecosystems/pypi/reserved-names.md)
- Publish, yank, and delete such a project: [host a verb-named project](@/ecosystems/pypi/guides/reserved-name.md)
- Delete a project named `yank` end to end: [delete a project named yank](@/ecosystems/pypi/tutorials/reserved-name.md)
