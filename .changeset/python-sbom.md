---
default: packaging
---

# Ship a PEP 770 SBOM from python-sbom in every wheel

Next to maturin's list of Rust crates, each wheel now carries a [PEP 770](https://peps.python.org/pep-0770/) CycloneDX
SBOM written by [python-sbom](https://github.com/tox-dev/python-sbom): it records the Python package, its supplier and
license, the build tools and the source revision, and links to maturin's file.
