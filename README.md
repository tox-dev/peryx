# <img src="assets/icon.svg" width="28" alt=""> peryx

**Fast as the falcon, sealed as the pyx.** peryx is one blazing-fast, open-source vault for a wide range of ecosystems:
a caching proxy of upstream indexes, a hosted store you publish to, and a virtual index that merges the two so your own
packages override upstream. It speaks PyPI (point pip, uv, or twine at it) and OCI (point docker or any registry client
at it) today, with more ecosystems arriving as drivers rather than rewrites. One async Rust process runs zero-config on
a laptop and scales to a cluster when configured.

```shell
cargo build --release
./target/release/peryx serve
uv pip install --index-url http://127.0.0.1:4433/root/pypi/simple/ requests
docker pull 127.0.0.1:4433/dockerhub/library/alpine
```

**Documentation: [peryx.readthedocs.io](https://peryx.readthedocs.io/)** - tutorials, how-to guides, the configuration
and endpoint reference, and design explanations. [proposal.md](proposal.md) holds the original design document and
roadmap; [CONTRIBUTING.md](CONTRIBUTING.md) covers development.

MIT licensed.
