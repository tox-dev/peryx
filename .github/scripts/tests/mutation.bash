#!/usr/bin/env bash
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd -P)
scratch=$(mktemp -d "${RUNNER_TEMP:-/tmp}/peryx-mutation.XXXXXX")
trap 'rm -rf "$scratch"' EXIT
mkdir -p "$scratch/bin" "$scratch/tmp"
cat >"$scratch/bin/git" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
case $1 in
  cat-file | diff) exit 0 ;;
  *) exit 2 ;;
esac
EOF
cat >"$scratch/bin/cargo" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%q ' "$@" >>"$MUTATION_TEST_LOG"
printf '\n' >>"$MUTATION_TEST_LOG"
if [[ " $* " == *' --list '* ]]; then
  for ((candidate = 1; candidate <= MUTATION_TEST_CANDIDATES; candidate++)); do
    printf 'src/lib.rs:%d:1: mutant %d\n' "$candidate" "$candidate"
  done
fi
EOF
chmod +x "$scratch/bin/git" "$scratch/bin/cargo"
export MUTATION_TEST_LOG=$scratch/calls
export PATH=$scratch/bin:$PATH
export RUNNER_TEMP=$scratch/tmp

export MUTATION_TEST_CANDIDATES=65
output=$(cd "$repo" && PERYX_MUTATION_BUDGET=32 .github/scripts/mutation-diff base 1 true)
grep -Fq 'mutation candidates=65 budget=32 part=1/1 shard=1/3' <<<"$output"
(("$(wc -l <"$MUTATION_TEST_LOG")" == 2))
grep -Fq -- '--workspace --all-features --in-diff' "$MUTATION_TEST_LOG"
tail -n 1 "$MUTATION_TEST_LOG" | grep -Fq -- '--baseline skip --output .tox/mutants --shard 1/3 --sharding round-robin --in-place'
if tail -n 1 "$MUTATION_TEST_LOG" | grep -Fq -- '--jobs'; then
  printf 'in-place mutation passed a job count\n' >&2
  exit 1
fi

: >"$MUTATION_TEST_LOG"
output=$(cd "$repo" && PERYX_MUTATION_BUDGET=32 .github/scripts/mutation-diff base 1 true 2/8)
grep -Fq 'mutation candidates=65 budget=32 part=2/8 shard=2/17' <<<"$output"
tail -n 1 "$MUTATION_TEST_LOG" | grep -Fq -- '--shard 2/17 --sharding round-robin --in-place'

: >"$MUTATION_TEST_LOG"
export MUTATION_TEST_CANDIDATES=20
output=$(cd "$repo" && PERYX_MUTATION_BUDGET=32 .github/scripts/mutation-diff base 1 true 8/8)
grep -Fq 'mutation candidates=20 budget=32 part=8/8 selected=0' <<<"$output"
(("$(wc -l <"$MUTATION_TEST_LOG")" == 1))

: >"$MUTATION_TEST_LOG"
export MUTATION_TEST_CANDIDATES=0
output=$(cd "$repo" && .github/scripts/mutation-diff base)
grep -Fq 'no changed production mutants' <<<"$output"
(("$(wc -l <"$MUTATION_TEST_LOG")" == 1))

if (cd "$repo" && PERYX_MUTATION_BUDGET=0 .github/scripts/mutation-diff base); then
  printf 'zero mutation budget passed\n' >&2
  exit 1
fi
if (cd "$repo" && .github/scripts/mutation-diff base 1 true 9/8); then
  printf 'invalid mutation part passed\n' >&2
  exit 1
fi

: >"$MUTATION_TEST_LOG"
(cd "$repo" && .github/scripts/mutation-full 2 3/8)
grep -Fq -- '--jobs 2 --output .tox/mutants --shard 3/8 --sharding round-robin' "$MUTATION_TEST_LOG"
