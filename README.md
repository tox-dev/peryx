# <img src="assets/icon.svg" width="28" alt=""> peryx

A blazing-fast artifact server written in Rust: a caching proxy of an upstream index, a hosted store you publish to, and
a virtual index that merges the two so local packages transparently override upstream. It speaks PyPI today (point pip,
uv, or twine at it), and its architecture is built to add more ecosystems without a rewrite. One async process runs
zero-config on a laptop and scales to a cluster when configured.

```shell
cargo build --release
./target/release/peryx serve
uv pip install --index-url http://127.0.0.1:4433/root/pypi/simple/ requests
```

**Documentation: [peryx.readthedocs.io](https://peryx.readthedocs.io/)** - tutorials, how-to guides, the
configuration and endpoint reference, and design explanations. [proposal.md](proposal.md) holds the original design
document and roadmap; [CONTRIBUTING.md](CONTRIBUTING.md) covers development.

MIT licensed.
