#!/usr/bin/env python3
from __future__ import annotations

import json
import re
import subprocess
import sys
from pathlib import Path
from typing import TypedDict


class Finding(TypedDict, total=False):
    code: str
    severity: str
    message: str
    file: str


class Package(TypedDict):
    name: str
    manifest_path: str


class Metadata(TypedDict):
    packages: list[Package]


class Report(TypedDict):
    findings: list[Finding]


def main() -> int:
    repo = Path(__file__).resolve().parents[2]
    report = json.loads(run(repo, "cargo", "shear", "--format", "json").stdout)
    metadata = json.loads(run(repo, "cargo", "metadata", "--no-deps", "--format-version", "1").stdout)
    roots = {package["name"]: Path(package["manifest_path"]).parent for package in metadata["packages"]}
    linked = linked_test_modules(repo)
    failed = False
    for raw_finding in report["findings"]:
        finding = raw_finding
        if finding["code"] == "shear/unlinked_files":
            finding = unlinked_test_finding(finding, roots, linked)
            if finding is None:
                continue
        if is_linked_test_dependency(finding, linked):
            continue
        location = f" ({finding['file']})" if finding.get("file") else ""
        sys.stderr.write(f"{finding['severity']}: {finding['code']}: {finding['message']}{location}\n")
        failed |= finding["severity"] == "error"
    return int(failed)


def run(repo: Path, *command: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(command, cwd=repo, check=False, capture_output=True, text=True)


def linked_test_modules(repo: Path) -> set[Path]:
    linked: set[Path] = set()
    pending: list[Path] = []
    for pattern in ("src/**/*.rs", "examples/**/*.rs", "benches/**/*.rs"):
        for source in repo.glob(f"crates/*/{pattern}"):
            pending.extend(path_modules(source))
    while pending:
        source = pending.pop().resolve()
        if source in linked or not source.is_file():
            continue
        linked.add(source)
        pending.extend(path_modules(source))
        pending.extend(plain_modules(source))
    return linked


def path_modules(source: Path) -> list[Path]:
    paths = re.findall(r'#\[path\s*=\s*"([^"]+)"\]', source.read_text(encoding="utf-8"))
    return [source.parent / path for path in paths]


def plain_modules(source: Path) -> list[Path]:
    base = (
        source.parent
        if source.name in {"lib.rs", "main.rs", "mod.rs"} or "tests" in source.parts
        else source.parent / source.stem
    )
    modules = re.findall(
        r"(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;",
        source.read_text(encoding="utf-8"),
    )
    return [candidate for module in modules for candidate in (base / f"{module}.rs", base / module / "mod.rs")]


def unlinked_test_finding(finding: Finding, roots: dict[str, Path], linked: set[Path]) -> Finding | None:
    lines = finding["message"].splitlines()
    package = lines[0].split("`")[1]
    unresolved = [path for path in lines[1:] if (roots[package] / path).resolve() not in linked]
    return {**finding, "message": "\n".join([lines[0], *unresolved])} if unresolved else None


def is_linked_test_dependency(finding: Finding, linked: set[Path]) -> bool:
    if finding["code"] not in {"shear/unused_dependency", "shear/unused_optional_dependency"}:
        return False
    manifest = finding.get("file")
    if manifest is None:
        return False
    root = Path(manifest).resolve().parent
    dependency = finding["message"].split("`")[1].replace("-", "_")
    pattern = re.compile(rf"(?<![A-Za-z0-9_]){re.escape(dependency)}(?![A-Za-z0-9_])")
    return any(source.is_relative_to(root) and pattern.search(source.read_text()) for source in linked)


if __name__ == "__main__":
    raise SystemExit(main())
