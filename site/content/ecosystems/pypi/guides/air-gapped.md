+++
title = "Serve a restricted or air-gapped network"
description = "peryx as the one approved path to PyPI, or as a warm-then-carry partial mirror when there is no path at all."
weight = 2
+++

A full PyPI mirror is double-digit terabytes, almost all of which nobody on your network will ever install. A
read-through cache is the practical alternative: a partial mirror containing exactly the packages your users have asked
for. Two topologies cover the common cases.

## Controlled egress: peryx as the choke point

The network allows outbound traffic only from approved hosts. Run peryx on one of them; everything else installs through
it and needs no internet route:

```toml
# peryx.toml on the egress host
host = "0.0.0.0"
port = 4433
data_dir = "/var/lib/peryx"
```

Clients set `PIP_INDEX_URL`/`UV_INDEX_URL` to `http://<egress-host>:4433/root/pypi/simple/` and are done. You get one
place to firewall, one place to [watch](@/core/monitor.md) (every download is counted per project and file), and one
place where [private packages shadow upstream](@/core/indexes.md).

If the egress host itself must go through a corporate proxy, standard `HTTPS_PROXY` environment variables apply to
peryx's upstream client.

## True air gap: warm, carry, serve

With no route at all, populate the cache on a connected network and move the data directory across the gap. For a
requirements-bounded mirror:

```shell
# connected side
peryx mirror plan root/pypi --data-dir ./peryx-data --requirements requirements.txt
peryx mirror sync root/pypi --data-dir ./peryx-data --requirements requirements.txt
peryx mirror verify root/pypi --data-dir ./peryx-data --requirements requirements.txt
```

Everything selected (pages, PEP 658 metadata, wheels, and sdists) now sits under `./peryx-data`. Create a backup, verify
it, carry the backup directory across the gap, restore it, and serve it in offline mode:

```shell
# connected side
peryx backup create --data-dir ./peryx-data ./peryx-backup
peryx backup verify ./peryx-backup

# isolated side
peryx restore ./peryx-backup --data-dir ./peryx-data
peryx serve --data-dir ./peryx-data --offline
```

The backup includes the metadata store, a config snapshot, and only the blob files referenced by metadata records.
Offline mode never tries the upstream. Artifacts and cached project pages serve straight from the store; a project or
file that was not carried over returns a resolver-visible miss. Repeat the warm-and-carry cycle whenever the requirement
set changes.

Resolve against a lock file (`uv.lock`, `requirements.txt` with hashes) on the connected side, so the isolated side asks
only for things the carry-over contains.

For a full upstream walk, use `--mode all` instead of a requirements file:

```shell
peryx mirror sync pypi --data-dir ./peryx-data --mode all
peryx mirror verify pypi --data-dir ./peryx-data --mode all
```

Full PyPI consumes many terabytes. Use `--python-tag`, `--abi-tag`, `--platform-tag`, and `--max-file-size-bytes` when
your clients need a bounded wheel set.

## What to check

- `curl http://<host>:4433/+status` shows the index list and counters.
- `curl http://<host>:4433/+status | jq '.indexes[].upstream?.offline'` shows which cached indexes run offline.
- `curl 'http://<host>:4433/+stats?index=root/pypi'` shows what the cache is serving.
- A `503` from a cached index route means a client asked for something the offline cache does not contain.
