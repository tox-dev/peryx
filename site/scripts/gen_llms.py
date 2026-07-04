"""Generate an llms.txt (llmstxt.org format) from the Zola content tree.

Zola cannot template arbitrary output files, so this runs after `zola build`: it reads the
front-matter of each content page and writes an LLM-friendly index of the documentation.

Usage: gen_llms.py --base-url URL --content DIR --out FILE
"""

from __future__ import annotations

import argparse
import re
from pathlib import Path
from typing import Final, NamedTuple

# Diátaxis order; the product/landing pages are deliberately excluded.
SECTIONS: Final = [
    ("tutorials", "Tutorials"),
    ("guides", "How-to guides"),
    ("reference", "Reference"),
    ("explanation", "Explanation"),
    ("migration", "Migration"),
]
TITLE_RE: Final = re.compile(r'^title\s*=\s*"(.*?)"', re.MULTILINE)
DESC_RE: Final = re.compile(r'^description\s*=\s*"(.*?)"', re.MULTILINE)
WEIGHT_RE: Final = re.compile(r"^weight\s*=\s*(\d+)", re.MULTILINE)


class Entry(NamedTuple):
    title: str
    url: str
    description: str
    weight: int


def front_matter(path: Path) -> tuple[str, str, int]:
    text = path.read_text(encoding="utf-8")
    if text.startswith("+++"):
        text = text.split("+++", 2)[1]
    title = TITLE_RE.search(text)
    desc = DESC_RE.search(text)
    weight = WEIGHT_RE.search(text)
    return (
        title.group(1) if title else path.stem,
        desc.group(1) if desc else "",
        int(weight.group(1)) if weight else 999,
    )


def collect(content: Path, base_url: str) -> tuple[str, list[tuple[str, list[Entry]]]]:
    _, root_desc, _ = front_matter(content / "_index.md")
    if (
        not root_desc
        and (config := content.parent / "config.toml").is_file()
        and (match := DESC_RE.search(config.read_text(encoding="utf-8")))
    ):
        root_desc = match.group(1)
    groups: list[tuple[str, list[Entry]]] = []
    for slug, label in SECTIONS:
        section = content / slug
        if not section.is_dir():
            continue
        entries = [
            Entry(t, f"{base_url}/{slug}/{md.stem}/", d, w)
            for md in sorted(section.glob("*.md"))
            if md.name != "_index.md"
            for t, d, w in [front_matter(md)]
        ]
        entries.sort(key=lambda e: (e.weight, e.title))
        if entries:
            groups.append((label, entries))
    return root_desc, groups


def render(base_url: str, root_desc: str, groups: list[tuple[str, list[Entry]]]) -> str:
    lines = [
        "# velodex",
        "",
        f"> {root_desc}",
        "",
        (
            "Documentation follows the Diátaxis framework: learning-oriented tutorials, "
            "task-oriented how-to guides, information-oriented reference, and "
            "understanding-oriented explanation."
        ),
        "",
    ]
    for label, entries in groups:
        lines.append(f"## {label}")
        for e in entries:
            suffix = f": {e.description}" if e.description else ""
            lines.append(f"- [{e.title}]({e.url}){suffix}")
        lines.append("")
    lines += ["## Optional", f"- [Contributing]({base_url}/contributing/)", ""]
    return "\n".join(lines)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--content", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    base_url = args.base_url.rstrip("/")
    root_desc, groups = collect(args.content, base_url)
    args.out.write_text(render(base_url, root_desc, groups), encoding="utf-8")
    print(f"wrote {args.out} ({sum(len(e) for _, e in groups)} pages)")  # noqa: T201 - build-step status line


if __name__ == "__main__":
    main()
