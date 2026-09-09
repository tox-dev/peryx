"""Report the source lines no monomorphization of their function executed, in any configuration.

`cargo llvm-cov --fail-under-lines` compares against LLVM's own line total, which is not the count of
lines nothing ran. LLVM sums a file's lines per function, and where a generic has several
monomorphizations it scores their instantiation group by taking the mapped count and the covered count
from whichever member is highest at each, on its own. A line one monomorphization runs and another
does not then counts as missed while executing. On this workspace the total reported 88 lines against
this check's 22.

`gaps` reads the native export and writes the lines no member of an instantiation group ran. `check`
reads that list and forgives a line where some configuration did run a function beginning on it, which
is how a body reached only from the browser reads: the four dashboard pages render their loader's
error only under `hydrate`, where the fetch behind it can fail, and `coverage-frontend` measures that
target. Forgiving by function start rather than by line hit is deliberate. A line hit says something
on the line ran, which a dead arm sharing a line with its covered guard would satisfy.
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


def executed_function_starts(lcov: str) -> dict[str, set[int]]:
    """File -> lines where a function that ran begins, read from an lcov tracefile."""
    starts: defaultdict[str, set[int]] = defaultdict(set)
    name = ""
    at: dict[str, int] = {}
    for raw in lcov.splitlines():
        if raw.startswith("SF:"):
            name, at = raw[3:], {}
        elif raw.startswith("FN:"):
            line, symbol = raw[3:].split(",", 1)
            at.setdefault(symbol, int(line))
        elif raw.startswith("FNDA:"):
            count, symbol = raw[5:].split(",", 1)
            if int(count) and symbol in at:
                starts[name].add(at[symbol])
    return starts


def report(gaps: dict[str, list[int]], headline: str) -> int:
    total = sum(len(lines) for lines in gaps.values())
    if not total:
        print("every mapped line ran in some monomorphization of its function")
        return 0
    print(f"{total} lines in {len(gaps)} files {headline}:")
    for name in sorted(gaps)[:_MAX_FILES]:
        listed = gaps[name][:_MAX_LISTED]
        more = "" if len(listed) == len(gaps[name]) else f" and {len(gaps[name]) - len(listed)} more"
        print(f"  {name}: {', '.join(str(line) for line in listed)}{more}")
    if len(gaps) > _MAX_FILES:
        print(f"  and {len(gaps) - _MAX_FILES} more files")
    return 1


def gaps_command(export_path: str, out_path: str) -> int:
    gaps = missed(json.loads(Path(export_path).read_text(encoding="utf-8")))
    Path(out_path).write_text(json.dumps(gaps, indent=2, sort_keys=True), encoding="utf-8")
    report(gaps, "ran in no monomorphization of their function here")
    return 0


def check_command(gaps_path: str, lcov_paths: list[str]) -> int:
    gaps = json.loads(Path(gaps_path).read_text(encoding="utf-8"))
    elsewhere: defaultdict[str, set[int]] = defaultdict(set)
    for path in lcov_paths:
        for name, lines in executed_function_starts(Path(path).read_text(encoding="utf-8")).items():
            elsewhere[name] |= lines
    remaining = {
        name: [line for line in lines if line not in elsewhere[name]]
        for name, lines in gaps.items()
        if [line for line in lines if line not in elsewhere[name]]
    }
    return report(remaining, "ran in no configuration")


def main(argv: list[str]) -> int:
    if argv[1:2] == ["gaps"] and len(argv) == 4:
        return gaps_command(argv[2], argv[3])
    if argv[1:2] == ["check"] and len(argv) >= 3:
        return check_command(argv[2], argv[3:])
    print(f"usage: {argv[0]} gaps <export.json> <out.json> | {argv[0]} check <out.json> [tracefile...]")
    return 2


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
