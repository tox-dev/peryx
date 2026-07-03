+++
title = "Serve a restricted or air-gapped network"
description = "velodex as the one approved path to PyPI, or as a warm-then-carry partial mirror when there is no path at all."
weight = 2
+++

A full PyPI mirror is double-digit terabytes, almost all of which nobody on your network will ever install. A
read-through cache is the practical alternative: a partial mirror containing exactly the packages your users have asked
for. Two topologies cover the common cases.

## Controlled egress: velodex as the choke point

The network allows outbound traffic only from approved hosts. Run velodex on one of them; everything else installs
through it and needs no internet route:

```toml
# velodex.toml on the egress host
host = "0.0.0.0"
port = 4433
data_dir = "/var/lib/velodex"
```

Clients set `PIP_INDEX_URL`/`UV_INDEX_URL` to `http://<egress-host>:4433/root/pypi/simple/` and are done. You get one
place to firewall, one place to [watch](@/guides/monitor.md) (every download is counted per project and file), and one
place where [private packages shadow upstream](@/explanation/indexes.md).

If the egress host itself must go through a corporate proxy, standard `HTTPS_PROXY` environment variables apply to
velodex's upstream client.

## True air gap: warm, carry, serve

With no route at all, populate the cache on a connected network and move the data directory across the gap:

```shell
# connected side: install the working set through a scratch velodex
velodex serve --data-dir ./velodex-data &
export UV_INDEX_URL=http://127.0.0.1:4433/root/pypi/simple/
uv pip install --dry-run -r requirements.txt   # resolve pulls pages and metadata
uv pip install -r requirements.txt             # download pulls the wheels
```

The installs leave pages, PEP 658 metadata, and wheels under `./velodex-data`. Create a backup, verify it, carry the
backup directory across the gap, and restore it on the isolated side:

```shell
# connected side
velodex backup create --data-dir ./velodex-data ./velodex-backup
velodex backup verify ./velodex-backup

# isolated side
velodex restore ./velodex-backup --data-dir ./velodex-data
velodex serve --data-dir ./velodex-data
```

The backup includes the metadata store, a config snapshot, and only the blob files referenced by metadata records.
Artifacts serve straight from the restored store. Cached pages past their freshness window serve stale when the upstream
is unreachable, which on an air-gapped network is the permanent state, so the index keeps answering with what was
carried over. Repeat the warm-and-carry cycle whenever the requirement set changes.

Resolve against a lock file (`uv.lock`, `requirements.txt` with hashes) on the connected side, so the isolated side asks
only for things the carry-over contains.

## What to check

- `curl http://<host>:4433/+status` shows the index list and counters.
- `curl 'http://<host>:4433/+stats?index=root/pypi'` shows what the cache is serving.
- The `stale_served` counter climbing on the gapped side is normal; `upstream_errors` above zero means a client asked
  for something the cache has never seen.
