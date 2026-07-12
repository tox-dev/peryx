+++
title = "Delete a project named yank"
description = "Publish a project whose name is yank, yank and un-yank it, then run the project-level delete that peryx once refused."
weight = 5
+++

`yank` is a real project name on pypi.org and a legal one under [PEP 503](https://peps.python.org/pep-0503/), yet it is
also the verb peryx puts in the URL that yanks a file. In this tutorial you publish a package named `yank` to a private
hosted index, yank and un-yank it, then delete it at the project level, the request the old router turned into a `400`.
It takes about five minutes.

## Start peryx with uploads

Save this config and start peryx. It serves a hosted store and a pypi.org cache behind one virtual route, `root/pypi`,
with an upload token:

```toml
# peryx.toml
[[index]]
name = "pypi"
cached = "https://pypi.org/simple/"

[[index]]
name = "hosted"
upload_token = "demo-secret"

[[index]]
name = "root/pypi"
layers = ["hosted", "pypi"]
upload = "hosted"
```

```shell
peryx serve --config peryx.toml
```

peryx listens on `127.0.0.1:4433`. Use a second terminal for the rest.

## Build a package named yank

Give a throwaway project the name `yank` and build a wheel:

```shell
mkdir yank-demo && cd yank-demo
cat > pyproject.toml <<'EOF'
[project]
name = "yank"
version = "1.0"
EOF
mkdir -p src/yank && touch src/yank/__init__.py
uv build
```

`dist/` now holds `yank-1.0-py3-none-any.whl` and its sdist.

## Publish it

```shell
uv publish --publish-url http://127.0.0.1:4433/root/pypi/ -u __token__ -p demo-secret dist/*
```

Confirm it resolves through the index:

```shell
curl -s http://127.0.0.1:4433/root/pypi/simple/yank/ | grep yank
```

## Yank and un-yank the project

The project name and the action word are both `yank`, so the project segment comes first and the action second:

```shell
# yank every file of the project
curl -X PUT -u __token__:demo-secret http://127.0.0.1:4433/root/pypi/yank/yank

# un-yank it
curl -X DELETE -u __token__:demo-secret http://127.0.0.1:4433/root/pypi/yank/yank
```

Each answers `200` with the number of files affected.

## Delete the project

Now the request that used to fail. Deleting the whole project addresses it with a trailing slash and no action word:

```shell
curl -X DELETE -u __token__:demo-secret http://127.0.0.1:4433/root/pypi/yank/
```

peryx returns `200` and the file count, and the project is gone:

```shell
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:4433/root/pypi/simple/yank/   # 404
```

Before the fix, peryx read the trailing `yank/` as an un-yank of a project with no name and answered `400`, leaving a
project named `yank` impossible to delete at the project level. The project segment in front of the action is what tells
peryx which one you mean.

## Related

- The full path table for `yank`, `restore`, and `promote`:
  [mutation paths for verb-named projects](@/ecosystems/pypi/reference/reserved-names.md)
- Manage a project named `restore` or `promote` too:
  [host a verb-named project](@/ecosystems/pypi/guides/reserved-name.md)
- Why peryx addresses these names at all: [verb-named projects](@/ecosystems/pypi/reserved-names.md)
