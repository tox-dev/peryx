"""Inline pre-rendered mermaid SVGs into the built HTML.

Zola emits each diagram as ``<pre class="mermaid">SOURCE</pre>``. This replaces every such block
with the committed dual-theme SVG partial that ``render_diagrams.mjs`` produced, keyed by a hash of
the diagram source. It touches no network and needs no browser, so it runs in the Read the Docs
build (which has Python but no headless Chrome).

A missing partial is a hard error: it means the committed diagrams are stale and CI's regeneration
step should have caught it.

Usage: python site/scripts/inline_diagrams.py <built-html-dir>
"""

from __future__ import annotations

import hashlib
import html
import re
import sys
from pathlib import Path
from typing import Final

BLOCK: Final = re.compile(r'<pre class="mermaid">(.*?)</pre>', re.DOTALL)


def diagram_key(pre_content: str) -> str:
    source = html.unescape(pre_content).strip()
    return hashlib.sha256(source.encode("utf-8")).hexdigest()[:16]


def inline_file(path: Path, partials: Path) -> int:
    text = path.read_text(encoding="utf-8")
    if 'class="mermaid"' not in text:
        return 0
    count = 0

    def replace(match: re.Match[str]) -> str:
        nonlocal count
        partial = partials / f"{diagram_key(match.group(1))}.html"
        if not partial.is_file():
            msg = f"{path}: no rendered diagram {partial.name}; run `npm --prefix site run render`"
            raise SystemExit(msg)
        count += 1
        return partial.read_text(encoding="utf-8").strip()

    replaced = BLOCK.sub(replace, text)
    if count:
        path.write_text(replaced, encoding="utf-8")
    return count


def main() -> None:
    if len(sys.argv) != 2:
        usage = "usage: inline_diagrams.py <built-html-dir>"
        raise SystemExit(usage)
    root = Path(sys.argv[1])
    partials = Path(__file__).resolve().parent.parent / "diagrams"
    total = sum(inline_file(path, partials) for path in root.rglob("*.html"))
    print(f"inlined {total} diagram(s) under {root}")


if __name__ == "__main__":
    main()
