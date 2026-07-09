+++
title = "Performance"
description = "peryx next to devpi, proxpi, pypiserver, and pypicloud: cold and warm installs, file throughput, a parallel CI fleet, and a request swarm, with the commands behind every number."
weight = 2
+++

Claims about speed are worthless without the commands behind them, so here are both. The headline: a **cold** install
through peryx costs about what going straight to pypi.org costs, and a **warm** one is bounded by the installer's own
CPU, not the network. This page measures that against every PyPI cache you could run instead. For why peryx behaves
this way, see [performance and methodology](@/core/performance.md).

## The measurement

The workload installs pandas and polars (six wheels, about 64 MB, including one 47 MB wheel) into a fresh virtualenv
with a fresh installer cache, so every byte must come through the index:

```shell
uv venv fresh-venv
env VIRTUAL_ENV=$PWD/fresh-venv UV_CACHE_DIR=$PWD/fresh-cache \
    UV_INDEX_URL=http://127.0.0.1:4433/root/pypi/simple/ \
    uv pip install pandas polars
```

Setup: peryx release build and the client on the same Apple Silicon laptop, roughly 700 Mbit/s to PyPI's CDN. Each
cell is the median over several independent rounds, each restarting the server on empty state; "cold" is that empty
first pass, "warm" reruns against the now-full cache. See [performance and methodology](@/core/performance.md) for how
the rounds, spread, and network-bound rows are handled.

| Scenario                    | Wall time   | What dominates                                     |
| --------------------------- | ----------- | -------------------------------------------------- |
| uv direct to pypi.org       | 0.94–1.03 s | the network, end to end                            |
| through peryx, cold cache | 1.13–1.38 s | the network; peryx adds ~0.1–0.3 s               |
| through peryx, warm cache | 0.66–0.71 s | uv itself (0.76 s of CPU unzipping and installing) |

Per-request server timings from the warm runs: simple pages and cached wheels serve in 0 ms; the largest page in the set
(numpy's, 2.6 MB of JSON) transforms in under 30 ms on its first warm hit and is a memory copy afterwards.

The run-to-run spread on the cold numbers is the CDN, not peryx: the same 47 MB wheel arrived in anything from 0.7 to
1.3 s across runs. And a laptop next to its cache is the *least* favorable setup for the warm numbers: the farther your
machines sit from PyPI (CI in a private subnet, an office behind one uplink), the more the warm path wins, because it
replaces your worst network hop instead of a loopback.

## The field

The tables below put peryx next to every alternative that starts hermetically from a package, plus **direct**, meaning
[uv](https://docs.astral.sh/uv/) talking to pypi.org with no proxy in between, the baseline every ratio compares
against. The servers overlap on features. They diverge on two things the benchmarks price: where the bytes go on a cache
miss, and what happens when many clients miss the same thing at once. The rest of this section reads each server's
source for those two axes, so the tables that follow are readable in advance rather than in hindsight.

| Server                                                 | Stack                                                                                                                                                                | On a miss                                                  | Persisted cache                                                     | Private uploads   |
| ------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------- | ------------------------------------------------------------------- | ----------------- |
| [peryx](@/core/architecture.md)                      | one static Rust binary, async ([tokio](https://tokio.rs/)/[axum](https://github.com/tokio-rs/axum)), one process                                                     | streams the bytes through, teeing into the store           | content-addressed, on disk ([redb](https://www.redb.org/) + blobs)  | token per index   |
| [devpi](https://devpi.net/docs/)                       | Python/[Pyramid](https://docs.pylonsproject.org/projects/pyramid/) on [waitress](https://docs.pylonsproject.org/projects/waitress/) (~50 threads); primary + replica | pages: fetch, parse, store, render; files: stream and tee  | [SQLite](https://www.sqlite.org/) keyfs plus sha256-addressed files | per-user, per-ACL |
| [proxpi](https://github.com/EpicWink/proxpi)           | Python/[Flask](https://flask.palletsprojects.com/) under [gunicorn](https://gunicorn.org/) (4 worker processes here)                                                 | download to a disk temp dir in a thread; client waits      | index in RAM (per worker), files on disk                            | none              |
| [pypiserver](https://github.com/pypiserver/pypiserver) | Python/[Bottle](https://bottlepy.org/docs/dev/), serves a directory of files                                                                                         | `302` redirect to pypi.org, caching nothing                | none for upstream content                                           | htpasswd on a dir |
| [pypicloud](https://pypicloud.readthedocs.io/)         | Python/Pyramid on waitress (8 threads); archived since 2023                                                                                                          | buffer the whole file to a temp file, store it, then serve | SQLite (or S3/GCS/DB) plus named-path files                         | user/group access |

### Where the bytes go on a miss

The cold rows come down to how each server moves an uncached wheel from pypi.org to the client.

{% mermaid() %}
flowchart TB
miss["client requests an uncached wheel"]
miss --> V["peryx, devpi:<br/>stream to the client while<br/>teeing into a content-addressed store"]
miss --> P["proxpi:<br/>download to a disk temp dir in a thread;<br/>client waits, or is redirected after 0.9 s"]
miss --> S["pypiserver:<br/>302 redirect to pypi.org;<br/>nothing is downloaded or cached"]
miss --> C["pypicloud:<br/>buffer the whole file to a temp file,<br/>write to store + DB, then serve"]
classDef good fill:#009E73,stroke:#009E73,color:#ffffff
classDef warn fill:#D55E00,stroke:#D55E00,color:#ffffff
class V good
class C,S warn
{% end %}

- **peryx** never buffers a whole response.
  [Page and artifact bytes stream to the client and into the store at once](@/core/architecture.md); peryx transforms
  a page chunk by chunk mid-flight, and tees a wheel to a temp file, hashes it, and renames it into the store once the
  client already has its bytes. A miss costs upstream wire time plus one hop. That sets the cold-install and
  cold-throughput numbers.
- **devpi** handles artifacts much as peryx does. `FileStreamer` writes each chunk to a local file and yields it to
  the client, then commits the sha256-addressed file once the body completes. Simple pages take the slower route: devpi
  fetches the upstream page, parses it, writes the link list into its SQLite keyfs, and only then renders a response
  from its own store. On PyPI-sized pages that parse-and-store step is real work on every refresh, and it runs under a
  single-writer transaction model.
- **proxpi** downloads a missed file to disk in a background thread while the requesting client blocks on
  `thread.join(0.9 s)`; if the download outruns that `PROXPI_DOWNLOAD_TIMEOUT`, proxpi redirects the client to pypi.org
  and lets the thread finish caching for next time. Its file cache defaults to a `tempfile.mkdtemp()` that gets deleted
  on shutdown, so without a configured `PROXPI_CACHE_DIR` the cache does not survive a restart. proxpi serves cached
  files from disk via `send_file`, not from an in-memory blob. The resident memory in the resource rows comes from four
  gunicorn worker processes, each holding its own unshared in-RAM index cache.
- **pypiserver** serves a directory of your own packages; with `--fallback-url` a miss is a bare `302` redirect to
  pypi.org's simple page. It downloads and caches nothing. That is why its CPU sits near zero and its cold and warm
  columns barely move: there is no cache to warm, and a miss is a formatted redirect string.
- **pypicloud** was the closest design to peryx (a `fallback = cache` read-through mirror), but its cold path fully
  buffers. It pulls the entire upstream file into a `TemporaryFile`, computes hashes, writes it to storage and a row
  into its cache DB, and only then sets the response body. The client waits for the download, the disk write, and the DB
  commit before its first byte. pypicloud stores files by `name/version/filename`, not by hash. The project has been
  archived since 2023 and runs only under Python 3.10 with [SQLAlchemy](https://www.sqlalchemy.org/) pinned below 2.

Only peryx serves [PEP 658](https://peps.python.org/pep-0658/) `.metadata` by default (and
[synthesizes it with byte-range reads](@/core/architecture.md) when an upstream lacks it); proxpi proxies it when the
upstream advertises it, devpi hides it behind an experimental `--enable-core-metadata`, and pypiserver and pypicloud do
not serve it at all. This drives the warm-resolution numbers: a resolver comparing ten versions fetches kilobytes from
peryx and megabytes of wheels from the servers that cannot offer the sibling.

### What a concurrent cold burst does

The cold rows of the parallel-install and throughput tables turn on one question: what does a server do when several
clients miss the *same* uncached thing at once? peryx answers with single-flight, where concurrent misses for one page
or file share a single upstream fetch and all tail its result. Two competitors answer with a failure the source
explains.

**devpi, the empty first page.** On the first concurrent fetch of a project, a request that loses the internal name-list
lock evaluates the project against an as-yet-empty project list, concludes it does not exist, returns a `404`, and
caches that negative result for the mirror-expiry window (30 minutes by default). uv reads the `404` as "no such
package" and the install fails.

{% mermaid() %}
sequenceDiagram
participant A as client A
participant B as clients B…J
participant D as devpi
A->>+D: GET simple/polars/ (first ever)
B->>+D: GET simple/polars/ (name list still empty)
D-->>-B: 404 "does not exist" (cached ~30 min)
Note over B: uv reads 404 as "no such package"
D-->>-A: 200 once the upstream fetch lands
{% end %}

**pypicloud, the concurrent INSERT.** The cache-on-miss path has no dedup and no locking. Four clients asking for one
wheel each download the whole file, then each try to write the same `filename` primary key into single-writer SQLite.
The commits serialize; the losers hit a `UNIQUE` constraint (or `database is locked`), and because
[pyramid_tm](https://docs.pylonsproject.org/projects/pyramid_tm/) commits after the view returns with no retry
configured, the exception surfaces as `HTTP 500`.

{% mermaid() %}
sequenceDiagram
participant C as 4 clients (cold)
participant P as pypicloud
participant S as SQLite
C->>+P: GET the same wheel ×4
Note over P: no dedup, each client<br/>downloads the whole file
P->>+S: INSERT the same filename ×4
S-->>-P: 2nd–4th: UNIQUE constraint / database is locked
P-->>-C: HTTP 500
{% end %}

Read this way, each table below is a controlled test of one axis: cold latency, warm overhead, a concurrent cold burst,
a fleet installing at once, a swarm reading pages. The architecture above says in advance which servers should struggle
where.

## The benchmark suite

The tables below come from a [benchmark harness](https://github.com/gaborbernat/peryx/tree/main/crates/peryx-bench) the
repository carries as a Rust crate: it builds peryx, starts every competitor from its published package, times the
same workload through each with a native HTTP client, samples each server's process tree while its workload runs, and
writes one TOML report these tables render from. Cells tint from best-in-row green to worst-in-row red; the ratio in
parentheses compares against **direct**, the no-proxy baseline, so each server's cell reads as the overhead (or win) it
adds over talking to pypi.org yourself.

The table covers every alternative that can be started hermetically from a published package: peryx, devpi, proxpi,
pypiserver (whose upstream fallback is a redirect rather than a cache), and pypicloud (archived upstream; it still runs,
but only under Python 3.10 with SQLAlchemy pinned below 2). Pulp needs PostgreSQL plus four services, nginx_pypi_cache
is a Docker configuration rather than a package, and Artifactory, Nexus, and the cloud registries need licenses or
accounts, so none of them can be measured this way.

The install workload is the top 51 most-downloaded PyPI packages
([the snapshot](https://github.com/gaborbernat/peryx/blob/main/crates/peryx-bench/src/ecosystems/pypi/packages.rs),
torch included for one large wheel), installed with uv into a fresh virtualenv with a fresh client cache. **Cold** is
the first install against a server with empty state; **warm** reruns it with the server's cache full and only the client
reset.

{{ bench(file="install-uv") }}

The same workload through pip tells a different story: pip installs serially and does its own work between requests, so
the client dominates and every server lands within a few seconds of the rest. A faster index cannot rescue a slow
client; through uv, the index is what you feel.

{{ bench(file="install-pip") }}

The throughput workload moves one large wheel (torch, ~88 MB). The cold row is the moment a CI fleet fears: four clients
ask for the same wheel the instant a release lands, and the server either fans one upstream transfer out to every waiter
or serializes them. peryx runs the transfer as a detached task every client tails, so all four see their first byte in
milliseconds and finish together in the time one download takes; pypicloud answers the same burst with HTTP 500. The hot
rows measure how fast a cached wheel leaves the server, alone and under eight parallel readers. Every number past ~3
GB/s outruns a 25 GbE link, so those cells compare server efficiency, not anything a client on a network would feel.

{{ bench(file="throughput") }}

The parallel-install workload is that fleet end to end: ten virtualenvs install polars at once, each with its own empty
client cache, exactly like ten CI jobs landing on the same runner pool. The server sees ten simultaneous copies of every
page and wheel request. This is where correctness under concurrency shows up next to speed: devpi fails eight of the ten
cold installs, because concurrent requests for a project it is fetching for the first time see an empty page and uv
concludes the package does not exist.

{{ bench(file="parallel-install") }}

The request workload drives a swarm against each warm server: one user, then 32, each a client that fetches project
pages and reads every byte of the body, the way a resolver does. The pages average ~480 KB, so this row prices full page
transfers, not header round-trips.

{{ bench(file="load") }}

Every table ends with two resource rows: the CPU seconds and peak resident memory of the server's whole process tree
while its workload ran, compared against peryx (direct runs no server, so it cannot anchor them). Speed alone hides a
trade: proxpi's hot-transfer lead comes from holding wheels in memory at three to five times peryx's footprint, and
pypiserver's near-zero CPU reflects that it redirects file downloads to PyPI instead of serving them.

Every server is measured the same way, on the same machine, in the same run, and one command reproduces every table:

```shell
cargo run --release -p peryx-bench
```

## Related

- Why the cold path keeps up and the warm path pulls ahead: [performance and methodology](@/core/performance.md)
- Put the cache in front of CI: [the CI guide](@/ecosystems/pypi/guides/ci-cache.md)
