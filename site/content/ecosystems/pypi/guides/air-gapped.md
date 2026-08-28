+++
title = "Serve a restricted or air-gapped network"
description = "Use peryx as the approved path to PyPI or carry a warmed partial mirror into an isolated network."
weight = 2
+++

A full PyPI mirror consumes tens of terabytes. Most networks use a fraction of its packages. A read-through cache holds
the packages that users request. Choose a controlled-egress or isolated topology.

## Controlled egress: peryx as the choke point

The network allows outbound traffic from approved hosts. Run peryx on one of them. Other hosts install through peryx
without an internet route:

```toml
# peryx.toml on the egress host
host = "0.0.0.0"
port = 4433
data_dir = "/var/lib/peryx"
```

Set `PIP_INDEX_URL` or `UV_INDEX_URL` to `http://<egress-host>:4433/root/pypi/simple/`. The egress host becomes the
firewall boundary and [monitoring point](@/core/operations/monitor.md). peryx counts downloads per project and file, and
the virtual route merges private and cached files. Add a
[project-isolation policy](@/ecosystems/pypi/reference/policy.md#project-isolation) for private names that must exclude
upstream versions.

If the egress host itself must go through a corporate proxy, standard `HTTPS_PROXY` environment variables apply to
peryx's upstream client.

## True air gap: warm, carry, serve

With no route at all, populate the cache on a connected network and move the data directory across the gap. For a
requirements-bounded mirror:

```shell
# connected side
peryx mirror plan root/pypi --data-dir ./peryx-data --option 'requirements=["requirements.txt"]'
peryx mirror sync root/pypi --data-dir ./peryx-data --option 'requirements=["requirements.txt"]'
peryx mirror verify root/pypi --data-dir ./peryx-data --option 'requirements=["requirements.txt"]'
```

peryx stores the selected pages, [PEP 658](https://peps.python.org/pep-0658/) metadata, wheels, and sdists under
`./peryx-data`. Create and verify a backup, carry it across the gap, restore it, and enable offline mode:

```shell
# connected side
peryx backup create --data-dir ./peryx-data ./peryx-backup
peryx backup verify ./peryx-backup

# isolated side
peryx restore ./peryx-backup --data-dir ./peryx-data
peryx serve --data-dir ./peryx-data --offline
```

The backup includes the metadata store, a configuration snapshot, and the blob files referenced by metadata records.
Offline mode blocks upstream requests. peryx serves artifacts and cached project pages from the store; requests for
content absent from the backup return a resolver-visible miss. Repeat the cycle after the requirement set changes.

Resolve a lock file (`uv.lock` or `requirements.txt` with hashes) on the connected side. The isolated side will then
request content from that lock.

Use `mode="all"` for a full upstream walk:

```shell
peryx mirror sync pypi --data-dir ./peryx-data --option 'mode="all"'
peryx mirror verify pypi --data-dir ./peryx-data --option 'mode="all"'
```

Full PyPI consumes many terabytes. Set `python_tags`, `abi_tags`, `platform_tags`, and `max_file_size_bytes` to limit
the wheel set.

## Verification

- `curl -u admin:"$ADMIN_PASSWORD" http://<host>:4433/+status | jq '.indexes[].upstream?.offline'` shows which cached
  indexes run offline; the index topology needs an administrator credential.
- `curl -u operator:"$OPERATOR_PASSWORD" 'http://<host>:4433/+stats?index=root/pypi'` shows what the cache is serving.
- A `503` from a cached index route means a client asked for something the offline cache does not contain.
