+++
template = "index.html"
+++

- **Read-through cache** Proxies pypi.org or any [PEP 503/691](https://packaging.python.org/en/latest/specifications/simple-repository-api/) index. A cache miss streams upstream bytes to
  the client while writing them aside, so the first install pays no penalty; every later one comes from disk. Each
  artifact verifies against its sha256 and is stored once, content-addressed, however many projects pull it.
- **Private packages that shadow upstream** twine and `uv publish` upload over the standard API into overlay
  indexes, where your `utils` beats the `utils` someone registered on pypi.org. One `index-url`, no
  `--extra-index-url`, no [dependency confusion](@/explanation/indexes.md).
- **Modern resolver protocols** PEP 691 JSON with PEP 503 HTML fallback, PEP 700 fields, and the PEP 658/714
  `.metadata` fast path that lets pip and uv resolve from kilobytes of metadata instead of whole wheels.
- **Honest freshness** Upstream `Cache-Control` decides how long a page serves from cache; a background sweep
  catches upstream changes even for pages nobody is requesting; outages degrade to stale-but-working.
- **Built to operate** One TOML file, Prometheus metrics, per-file usage drill-down, structured logs, a live web
  UI, and a data directory you can back up with `cp`. No JVM, no database server, idle RAM in the tens of MB.
- **Proven with real clients** The test suite drives actual pip, uv, and twine against a live velodex, holds 100%
  line and function coverage, and the [performance numbers](@/explanation/performance.md) come with the exact
  commands that produced them.
