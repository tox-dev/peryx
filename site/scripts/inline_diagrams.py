from __future__ import annotations

import hashlib
import html
import re
import sys
from pathlib import Path
from typing import Final

BLOCK: Final = re.compile(r'<pre class="mermaid">(.*?)</pre>', re.DOTALL)


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit("usage: inline_diagrams.py <built-html-dir>")
    partials = diagram_partials(Path(__file__).resolve())
    sum(inline_file(path, partials) for path in Path(sys.argv[1]).rglob("*.html"))


def inline_file(path: Path, partials: dict[str, Path]) -> int:
    text = path.read_text(encoding="utf-8")
    if 'class="mermaid"' not in text:
        return 0
    count = 0

    def replace(match: re.Match[str]) -> str:
        nonlocal count
        key = diagram_key(match.group(1))
        if (partial := partials.get(key)) is None:
            raise SystemExit(f"{path}: no rendered diagram {key}.html; run `just docs`")
        count += 1
        return partial.read_text(encoding="utf-8").strip()

    replaced = BLOCK.sub(replace, text)
    if count:
        path.write_text(replaced, encoding="utf-8")
    return count


def diagram_partials(script: Path) -> dict[str, Path]:
    site = script.parent.parent
    directories = [site / "diagrams"]
    if (repository := repository_root(site)) is not None:
        directories.extend(
            declaration.parent / "diagrams"
            for declaration in sorted((repository / "crates").glob("*/docs/ecosystem.toml"))
        )
    partials: dict[str, Path] = {}
    for directory in directories:
        if not directory.is_dir():
            continue
        for path in sorted(directory.glob("*.html")):
            if (previous := partials.get(path.stem)) is not None:
                raise SystemExit(f"duplicate rendered diagram {path.stem}: {previous} and {path}")
            partials[path.stem] = path
    return partials


def repository_root(site: Path) -> Path | None:
    return next((parent for parent in site.parents if (parent / "crates").is_dir()), None)


def diagram_key(pre_content: str) -> str:
    return hashlib.sha256(html.unescape(pre_content).strip().encode()).hexdigest()[:16]


if __name__ == "__main__":
    main()
