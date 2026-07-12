+++
title = "Verb-named projects"
description = "Why peryx hosts and deletes a project whose name is a mutation verb like yank, and the addressing bug the old routing caused."
weight = 6
+++

peryx serves three mutations, yank and restore and promote, and names each one in the URL that performs it. A project
whose name is one of those words is a legal package, and peryx now addresses it like any other. This page explains why
the name and the verb must not share fate, and the delete that used to be impossible.

## peryx does not reserve names

peryx is a private index and a mirror. It hosts whatever [PEP 503](https://peps.python.org/pep-0503/)-legal name you
push and whatever a cached upstream carries, and it keeps no list of prohibited or reserved project names. Blocking
names is a public-registry concern: pypi.org withholds some names to keep an open, shared namespace legible and to blunt
squatting. Inside your own index the namespace is yours, and `yank` is as valid a project as `requests`.

What the old router did have was an *accidental* reservation. The mutation URLs reuse the verbs as path segments, and
the routing peeled a trailing `yank`, `restore`, or `promote` off the path before it read the project name. When the
verb was the entire path, nothing was left to name the project, so the three verbs went missing as project-addressable
names, a side effect of the grammar rather than a rule anyone wrote.

## The failure it prevents

`DELETE /root/pypi/yank/` deletes the project `yank`. The old router read the trailing `yank` as the un-yank action,
looked for a project name in front of it, found none, and rejected the request with `400 Bad Request`. A project named
`yank` could be uploaded and installed but never deleted at the project level: its name shadowed the delete.

`yank` and `restore` are real projects on pypi.org, so the collision was reachable. It bit where peryx is meant to
disappear:

- **Mirroring.** A cached index pulls a project named `yank` from pypi.org into a virtual index, and the operator cannot
  later remove it from the hosted layer.
- **Migrating.** A team moving a back catalogue onto peryx re-uploads a package named `restore`, then finds it stuck: no
  project-level delete to undo a mistaken import.

An index that cannot delete a project it accepted is not a drop-in front for one that can.

## Where the line is now

peryx separates the two namespaces by position, not by forbidding the name. A trailing verb is an action only when a
project segment precedes it; a path that is nothing but the verb names the project. `DELETE /root/pypi/yank/` deletes
`yank`, `PUT /root/pypi/yank/yank` yanks it, and the versioned and normal project-level forms are unchanged. The
[reference](@/ecosystems/pypi/reference/reserved-names.md) lists every path.

This does not loosen anything. A real yank still needs its `.../yank` suffix behind a project, a real delete still needs
the token and a volatile hosted layer. peryx stopped treating a lone verb as an action; it did not stop treating a
suffixed verb as one.

## In practice

- The exact paths for each verb-named project:
  [mutation paths for verb-named projects](@/ecosystems/pypi/reference/reserved-names.md)
- Host and manage such a project: [host a verb-named project](@/ecosystems/pypi/guides/reserved-name.md)
- Delete a project named `yank` step by step:
  [delete a project named yank](@/ecosystems/pypi/tutorials/reserved-name.md)
