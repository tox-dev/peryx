+++
title = "Performance"
description = "Cold and warm installs, file throughput, parallel CI, and request throughput for peryx, devpi, proxpi, pypiserver, and pypicloud."
weight = 2
+++

Each result includes its command. A **cold** install through peryx costs about the same as a direct pypi.org install. A
**warm** install is faster than pypi.org and bounded by installer CPU instead of network latency. See
[performance and methodology](@/core/performance.md) for the shared test controls.

{{ machine(owner="pypi") }}

## Benchmark setup

The workload installs the 51 most-downloaded packages on PyPI, torch among them for one large wheel, into a fresh
virtualenv with a fresh installer cache, so every byte must come through the index:

```shell
uv venv --python 3.14 fresh-venv
env VIRTUAL_ENV=$PWD/fresh-venv UV_CACHE_DIR=$PWD/fresh-cache \
    uv pip install --index-url http://127.0.0.1:4433/root/pypi/simple/ \
        --only-binary :all: boto3 urllib3 botocore requests numpy pandas ... torch
```

`--only-binary :all:` keeps the run honest. Without it a package that ships no wheel for the interpreter is compiled
from source, and that build lands inside the measured install, dwarfing anything the index server contributes.

Setup: peryx release build and the client on the same Apple Silicon machine, roughly 700 Mbit/s to PyPI's CDN. Each cell
is the median over three independent rounds, each restarting the server on empty state; "cold" is that empty first pass,
"warm" reruns against the now-full cache. Every server is torn down with its whole process group between rounds, so a
forked worker cannot outlive its round and steal CPU from whoever is measured next. See
[performance and methodology](@/core/performance.md) for how the rounds, spread, and network-bound rows are handled.

| Scenario                  | Wall time | What dominates                             |
| ------------------------- | --------- | ------------------------------------------ |
| uv direct to pypi.org     | 3.7 s     | the network, end to end                    |
| through peryx, cold cache | 4.4 s     | the network; peryx adds about one hop      |
| through peryx, warm cache | 3.3 s     | uv itself, unzipping and installing wheels |

An install is a blunt instrument for measuring an index server. uv's own resolve, unzip, and install work dominates the
wall clock, so every cache lands within a second or two of the others, and a faster index cannot rescue a slow client.
The rows that actually isolate the server are the request swarm and the file throughput further down, where peryx
answers 571 requests a second against direct's 215, holds a 9 ms p95, and serves a thousand of those requests on 2.6 s
of CPU where devpi needs 13.4 s.

A laptop next to its cache is the *least* favorable setup for the warm numbers: the farther your machines sit from PyPI
(CI in a private subnet, an office behind one uplink), the more the warm path wins, because it replaces your worst
network hop instead of a loopback.

## Compared servers

The comparison includes every alternative that starts hermetically from a package. **Direct** means
[uv](https://docs.astral.sh/uv/) talking to pypi.org without a proxy and provides the baseline for each ratio. The
benchmarks measure cache-miss data paths and concurrent misses. The comparison derives both behaviors from each server's
source.

| Server                                                 | Stack                     | On a miss                          | Persisted cache                          | Private uploads         |
| ------------------------------------------------------ | ------------------------- | ---------------------------------- | ---------------------------------------- | ----------------------- |
| [peryx](@/core/architecture.md)                        | Rust, Tokio, Axum         | Streams to the client and store    | redb plus content-addressed blobs        | Scoped tokens per index |
| [devpi](https://devpi.net/docs/)                       | Python, Pyramid, waitress | Parses pages; streams files        | SQLite keyfs plus sha256-addressed files | Per-user ACL            |
| [proxpi](https://github.com/EpicWink/proxpi)           | Python, Flask, gunicorn   | Downloads to a temporary directory | In-memory index and files on disk        | None                    |
| [pypiserver](https://github.com/pypiserver/pypiserver) | Python, Bottle            | Redirects to pypi.org              | None                                     | htpasswd per directory  |
| [pypicloud](https://pypicloud.readthedocs.io/)         | Python, Pyramid, waitress | Buffers, stores, then serves       | SQLite or remote metadata plus files     | User and group access   |

### Cache-miss data path

The cold rows measure how each server moves an uncached wheel from pypi.org to the client.

{% mermaid() %} flowchart TB miss["client requests an uncached wheel"] miss --> V\["peryx, devpi:<br/>stream to the
client while<br/>teeing into a content-addressed store"\] miss --> P\["proxpi:<br/>download to a disk temp dir in a
thread;<br/>client waits, or is redirected after 0.9 s"\] miss --> S\["pypiserver:<br/>302 redirect to
pypi.org;<br/>nothing is downloaded or cached"\] miss --> C\["pypicloud:<br/>buffer the whole file to a temp
file,<br/>write to store + DB, then serve"\] class V good class C,S warn {% end %}

- **peryx** never buffers a whole response.
  [Page and artifact bytes stream to the client and into the store at once](@/core/architecture.md); peryx transforms a
  page chunk by chunk mid-flight, and tees a wheel to a temp file, hashes it, and renames it into the store once the
  client already has its bytes. A miss costs upstream wire time plus one hop. That sets the cold-install and
  cold-throughput numbers.
- **devpi** handles artifacts much as peryx does. `FileStreamer` writes each chunk to a local file and yields it to the
  client, then commits the sha256-addressed file once the body completes. Simple pages take the slower route: devpi
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

### Concurrent cold bursts

The parallel-install and throughput cold rows send concurrent requests for the same uncached object. peryx uses
single-flight, so misses for one page or file share one upstream fetch. Two competitors fail under this load.

**devpi, the empty first page.** On the first concurrent fetch of a project, a request that loses the internal name-list
lock evaluates the project against an as-yet-empty project list, concludes it does not exist, returns a `404`, and
caches that negative result for the mirror-expiry window (30 minutes by default). uv reads the `404` as "no such
package" and the install fails.

{% mermaid() %} sequenceDiagram participant A as client A participant B as clients B…J participant D as devpi A->>+D:
GET simple/polars/ (first ever) B->>+D: GET simple/polars/ (name list still empty) D-->>-B: 404 "does not exist" (cached
\~30 min) Note over B: uv reads 404 as "no such package" D-->>-A: 200 once the upstream fetch lands {% end %}

**pypicloud, the concurrent INSERT.** The cache-on-miss path has no dedup and no locking. Four clients asking for one
wheel each download the whole file, then each try to write the same `filename` primary key into single-writer SQLite.
The commits serialize; the losers hit a `UNIQUE` constraint (or `database is locked`), and because
[pyramid_tm](https://docs.pylonsproject.org/projects/pyramid_tm/) commits after the view returns with no retry
configured, the exception surfaces as `HTTP 500`.

{% mermaid() %} sequenceDiagram participant C as 4 clients (cold) participant P as pypicloud participant S as SQLite
C->>+P: GET the same wheel ×4 Note over P: no dedup, each client<br/>downloads the whole file P->>+S: INSERT the same
filename ×4 S-->>-P: 2nd-4th: UNIQUE constraint / database is locked P-->>-C: HTTP 500 {% end %}

Read this way, each table below is a controlled test of one axis: cold latency, warm overhead, a concurrent cold burst,
a fleet installing at once, a swarm reading pages. The architecture above says in advance which servers should struggle
where.

## Benchmark suite

The neutral [benchmark runner](https://github.com/tox-dev/peryx/tree/main/crates/peryx-bench) builds peryx, starts each
competitor, runs one workload through a native HTTP client, samples each process tree, and writes the TOML reports
rendered here. The PyPI crate owns the workload and server adapters in `crates/peryx-ecosystem-pypi/src/bench/` and
exposes them through `peryx-bench-pypi`. Cell colors rank each row. Parenthesized ratios compare each server with the
no-proxy **direct** baseline.

### Running benchmarks

Inspect the PyPI benchmark command, run one local target, or run the complete CodSpeed package lane:

```shell
cargo run -p peryx-ecosystem-pypi --features bench --bin peryx-bench-pypi -- --help
cargo bench --locked -p peryx-ecosystem-pypi --bench parse
just codspeed peryx-ecosystem-pypi
ci/run-codspeed-local.sh peryx-ecosystem-pypi
```

### Root-catalog synchronization

The root-catalog benchmark generates exactly one million valid project names, serves one PEP 691 response over loopback,
and runs the same streaming parser, 10,000-name transactions, and generation swap as `mirror sync --mode all`. While the
sync runs, another runtime worker repeatedly reads an unrelated project record. The report includes wall time, upstream
request count, completed foreground reads, and their p99 latency.

Build once before collecting samples, then run at least five measured rounds. Compare medians from the same machine and
power state; do not compare a debug build, the first build, or results from hosts with different storage. On macOS,
`/usr/bin/time -l` reports peak resident memory as `maximum resident set size`; GNU time reports it as
`Maximum resident set size`:

```shell
CARGO_TARGET_DIR=.tox/target-catalog cargo build --release \
  -p peryx-ecosystem-pypi --example catalog_million

/usr/bin/time -l .tox/target-catalog/release/examples/catalog_million
# Linux: /usr/bin/time -v .tox/target-catalog/release/examples/catalog_million
```

The fixture lives in the benchmark process, so peak RSS includes its generated JSON and the loopback server's response
buffer. Compare deltas only between revisions of this benchmark. The one-request assertion catches accidental refetches;
the project-count assertion catches partial publication. CI should use a fixed runner image and upload the raw stdout
and time output rather than enforcing a cross-machine wall-time threshold.

The table covers every alternative that can be started hermetically from a published package: peryx, devpi, proxpi,
pypiserver (whose upstream fallback is a redirect rather than a cache), and pypicloud (archived upstream; it still runs,
but only under Python 3.10 with SQLAlchemy pinned below 2). [Pulp](https://pulpproject.org/) needs
[PostgreSQL](https://www.postgresql.org/) plus four services,
[nginx_pypi_cache](https://github.com/hauntsaninja/nginx_pypi_cache) is a [Docker](https://www.docker.com/)
configuration rather than a package, and [Artifactory](https://jfrog.com/artifactory/),
[Nexus](https://www.sonatype.com/products/nexus-repository), and the cloud registries need licenses or accounts, so none
of them can be measured this way.

The install workload is the top 51 most-downloaded PyPI packages (the snapshot is in
`crates/peryx-ecosystem-pypi/src/bench/packages.rs`; torch included for one large wheel), installed with uv into a fresh
virtualenv with a fresh client cache. **Cold** is the first install against a server with empty state; **warm** reruns
it with the server's cache full and only the client reset.

{{ bench(file="install-uv", owner="pypi") }}

The same workload through [pip](https://pip.pypa.io/) tells a different story: pip installs serially and does its own
work between requests, so the client dominates and every server lands within a few seconds of the rest. A faster index
cannot rescue a slow client; through uv, the index is what you feel.

{{ bench(file="install-pip", owner="pypi") }}

The throughput workload moves one large wheel (torch, ~88 MB). The cold row is the moment a CI fleet fears: four clients
ask for the same wheel the instant a release lands, and the server either fans one upstream transfer out to every waiter
or serializes them. peryx runs the transfer as a detached task every client tails, so all four see their first byte in
milliseconds and finish together in the time one download takes; pypicloud answers the same burst with HTTP 500. The hot
rows measure how fast a cached wheel leaves the server, alone and under eight parallel readers. Every number past ~3
GB/s outruns a 25 GbE link, so those cells compare server efficiency, not anything a client on a network would feel.

{{ bench(file="throughput", owner="pypi") }}

The parallel-install workload is that fleet end to end: ten virtualenvs install polars at once, each with its own empty
client cache, exactly like ten CI jobs landing on the same runner pool. The server sees ten simultaneous copies of every
page and wheel request. This is where correctness under concurrency shows up next to speed: devpi fails eight of the ten
cold installs, because concurrent requests for a project it is fetching for the first time see an empty page and uv
concludes the package does not exist.

{{ bench(file="parallel-install", owner="pypi") }}

The request workload drives a swarm against each warm server: one user, then 32, each a client that fetches project
pages and reads every byte of the body, the way a resolver does. The pages average ~480 KB, so this row prices full page
transfers, not header round-trips. Every client sends the `Accept` header pip and uv send, because peryx picks the
representation from it: a swarm asking for `*/*` receives the [PEP 503](https://peps.python.org/pep-0503/) HTML render
instead, which is not the page an installer ever gets, and prices work no install performs.

peryx answers **7,658 requests a second** to a single client at a 2.3 ms p95, and **17,707** to thirty-two at 3.8 ms;
the next-fastest local cache manages 176 and 752. The gap is the cached page: a warm hit is a lookup and a copy of bytes
peryx has already transformed, so it serves a thousand of those requests on **168 ms of CPU** where devpi spends 9.6 s
and proxpi 6.3 s.

{{ bench(file="load", owner="pypi") }}

Every table ends with two resource rows: the CPU the server's whole process tree burned while its workload ran, and its
peak resident memory, compared against peryx (direct runs no server, so it cannot anchor them). The load table prices
that CPU **per thousand requests served**, which is the only way to read it. A fixed-duration swarm hands the slowest
server the smallest bill, and unnormalized it rewards failure: devpi answers 114 requests a second to peryx's 17,707, so
its absolute CPU looks modest until you divide by the work done.

Read the ratios against `direct` with care here. Every other party is a process on this machine, while `direct` is
pypi.org across the internet, so its rows carry a wide-area round trip no local cache pays.

Speed alone hides a trade. proxpi's eight-way transfer lead comes from holding wheels in memory at nearly seven times
peryx's 44 MB footprint, and pypiserver's near-zero CPU reflects that it redirects file downloads to PyPI instead of
serving them.

## Endpoint coverage

The client workloads cover the project page, wheel, and PEP 658 metadata endpoints. The endpoint benchmark measures one
warm request to every other served endpoint.

It is peryx against itself, not against the field. A PyPI server chooses its own url shapes and decides what its index
root contains: pypi.org answers `/pypi/{project}/json` where peryx answers `{index}/{project}/json`, devpi addresses
files by an internal path, and a proxy's index root lists what it has cached while pypi.org's lists every project that
exists. Rows across those servers would compare different work and read as a ranking. The comparisons live in the tables
above, which drive one client against everyone.

{{ bench(file="endpoints", owner="pypi") }}

Two rows stand out, and both are the same fact. The JSON project page is served from the transformed-page cache, so it
costs a lookup and a copy. The HTML render and the legacy `/{project}/json` API are not cached at all: each request
parses the stored page and renders it again, which is why HTML costs about nine times the JSON page and the legacy API
about twenty. Installers ask for JSON, so no install pays this; a browser and an old client do.

Every server is measured the same way, on the same machine, in the same run, and one command reproduces every table: see
[run the benchmarks](@/contributing/benchmarking.md).

## Related

- Benchmark controls and interpretation: [performance and methodology](@/core/performance.md)
- Put the cache in front of CI: [the CI guide](@/guides/ci-cache.md)
