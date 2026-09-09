"""Report the source lines no monomorphization of their function executed.

`cargo llvm-cov --fail-under-lines` compares against LLVM's own line total, which is not the number
of lines nothing ran. LLVM sums a file's lines per function, and where a generic has several
monomorphizations it scores their instantiation group by taking the mapped count and the covered
count from whichever member is highest at each, independently. A line one monomorphization runs and
another does not is therefore counted missed while executing. On this workspace that described 66 of
88 reported lines, and closing them would mean changing which concrete type a test happens to pick.

This takes the union across a group instead. A line stays missed only when no member of its
instantiation group ran it, which keeps the gate on unexecuted code and takes the merge artefact out.
"""

from __future__ import annotations

import json
import sys
from collections import defaultdict
from pathlib import Path

_SKIPPED_REGION = 2
_GAP_REGION = 3
_MAX_LISTED = 40
_MAX_FILES = 25


def line_counts(regions: list[list[int]]) -> dict[int, int]:
    """Line -> execution count for one function record, over its own file.

    This follows LLVM: a line takes the highest count of the regions starting on it, and a line
    carrying none takes the count of a region spanning it, which is how a continuation line of a
    multi-line expression reads.
    """
    own = [r for r in regions if r[5] == 0 and r[7] != _SKIPPED_REGION]
    counts: dict[int, int] = {}
    for start, _, end, _, count, _, _, kind in own:
        if kind != _GAP_REGION:
            counts[start] = max(counts.get(start, 0), count)
        for line in range(start, end + 1):
            counts.setdefault(line, 0)
    for start, _, end, _, count, _, _, _ in own:
        for line in range(start, end + 1):
            if counts[line] == 0:
                counts[line] = count
    return counts


def missed(export: dict) -> dict[str, list[int]]:
    """File -> lines no member of their instantiation group executed."""
    groups: defaultdict[tuple[str, int], list[dict[int, int]]] = defaultdict(list)
    for data in export["data"]:
        # The report's own file list is what `--ignore-filename-regex` narrowed, and a function
        # record carries no such filter, so a dependency's generic instantiated here would
        # otherwise be reported against its own source.
        reported = {file["filename"] for file in data["files"]}
        for function in data["functions"]:
            if not function["filenames"] or function["filenames"][0] not in reported:
                continue
            counts = line_counts(function["regions"])
            if counts:
                # LLVM keys an instantiation group by where the function starts, so every
                # monomorphization of one generic lands in the same bucket.
                groups[function["filenames"][0], min(counts)].append(counts)
    gaps: defaultdict[str, set[int]] = defaultdict(set)
    for (name, _), members in groups.items():
        covered = {line for member in members for line, count in member.items() if count}
        mapped = {line for member in members for line in member}
        gaps[name] |= mapped - covered
    return {name: sorted(lines) for name, lines in gaps.items() if lines}


def main(path: str) -> int:
    gaps = missed(json.loads(Path(path).read_text(encoding="utf-8")))
    total = sum(len(lines) for lines in gaps.values())
    if not total:
        print("every mapped line ran in some monomorphization of its function")
        return 0
    print(f"{total} lines in {len(gaps)} files ran in no monomorphization of their function:")
    for name in sorted(gaps)[:_MAX_FILES]:
        listed = gaps[name][:_MAX_LISTED]
        more = "" if len(listed) == len(gaps[name]) else f" and {len(gaps[name]) - len(listed)} more"
        print(f"  {name}: {', '.join(str(line) for line in listed)}{more}")
    if len(gaps) > _MAX_FILES:
        print(f"  and {len(gaps) - _MAX_FILES} more files")
    return 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1]))
