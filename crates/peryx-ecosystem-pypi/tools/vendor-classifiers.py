# /// script
# requires-python = ">=3.13"
# dependencies = ["trove-classifiers==2026.6.1.19"]
# ///
"""Vendor Warehouse classifiers so upload validation works offline."""

from __future__ import annotations

import subprocess
from importlib.metadata import version
from pathlib import Path

from trove_classifiers import deprecated_classifiers, sorted_classifiers


def main() -> None:
    known = sorted(sorted_classifiers)
    deprecated = sorted(deprecated_classifiers.items())
    lines = [
        "//! The trove classifier set `PyPI` validates uploads against.",
        "//!",
        f"//! Generated from trove-classifiers {version('trove-classifiers')} by `tools/vendor-classifiers.py`.",
        "",
        "/// Classifiers `PyPI` accepts, sorted so a refresh shows up as a clean diff.",
        f"pub(super) const KNOWN: [&str; {len(known)}] = [",
        *(f'    "{value}",' for value in known),
        "];",
        "",
        "/// Classifiers `PyPI` rejects as deprecated, each with the reason it reports.",
        f"pub(super) const DEPRECATED: [(&str, &str); {len(deprecated)}] = [",
        *(f'    ("{value}", "{deprecation_reason(replacements)}"),' for value, replacements in deprecated),
        "];",
        "",
    ]
    out = Path(__file__).parent.parent / "src" / "classifier" / "data.rs"
    out.write_text("\n".join(lines))
    subprocess.run(["rustfmt", "--edition", "2024", str(out)], check=True)


def deprecation_reason(replacements: list[str]) -> str:
    if not replacements:
        return "is deprecated"
    return f"is deprecated; use {' or '.join(replacements)} instead"


if __name__ == "__main__":
    main()
