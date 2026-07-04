# <img src="assets/icon.svg" width="28" alt=""> velodex

A PyPI-compatible read-through cache and private index, written in Rust. Point pip, uv, or twine at velodex: it caches
pypi.org (or any upstream), hosts your own uploads, and serves both from one virtual index through the wire protocols the
clients already speak.

```shell
cargo build --release
./target/release/velodex serve
uv pip install --index-url http://127.0.0.1:4433/root/pypi/simple/ requests
```

**Documentation: [velodex.readthedocs.io](https://velodex.readthedocs.io/)** - tutorials, how-to guides, the
configuration and endpoint reference, and design explanations. [proposal.md](proposal.md) holds the original design
document and roadmap; [CONTRIBUTING.md](CONTRIBUTING.md) covers development.

MIT licensed.
