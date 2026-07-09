+++
title = "Yank and delete packages"
description = "Yank an uploaded release per PEP 592, or delete it outright."
weight = 6
+++

Both operations take the same Basic-auth token as uploads, and both work on upstream files too: a cached index is
read-only, so peryx records the change as a reversible override on the virtual index's hosted layer instead of
touching the cached index. Removing an uploaded file that shadowed an upstream one makes the upstream version visible
again.

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
`volatile = false` on release indexes you want immutable, and peryx answers `403` instead. Deleting files that come
from a cached index hides them from the virtual index reversibly; `restore` undoes it.

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

## Related

- Yank vs delete vs hide, and why all three exist: [the index model](@/core/indexes.md)
- The same actions from the browser: [the web UI](@/core/web-ui.md)
