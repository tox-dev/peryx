#!/usr/bin/env bash
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd -P)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

track_all() {
  (cd "$1" && git init -q && git add .)
}

mkdir -p "$scratch/path-mounted/crates/nested/example/src" \
  "$scratch/path-mounted/crates/nested/example/tests/unit"
cat >"$scratch/path-mounted/Cargo.toml" <<'EOF'
[workspace]
members = ["crates/nested/example"]
resolver = "2"
EOF
cat >"$scratch/path-mounted/crates/nested/example/Cargo.toml" <<'EOF'
[package]
name = "example"
version = "0.0.0"
edition = "2021"
EOF
cat >"$scratch/path-mounted/crates/nested/example/src/lib.rs" <<'EOF'
#[cfg(test)]
#[path = "../tests/unit/tests.rs"]
mod tests;
EOF
printf '#[test]\nfn external_test() {}\n' >"$scratch/path-mounted/crates/nested/example/tests/unit/tests.rs"
track_all "$scratch/path-mounted"
mkdir "$scratch/no-cargo-bin"
for command in awk bash basename dirname find git sort; do
  ln -s "$(command -v "$command")" "$scratch/no-cargo-bin/$command"
done
(cd "$scratch/path-mounted" && PATH="$scratch/no-cargo-bin" "$repo/.github/scripts/check-test-layout")

mkdir -p "$scratch/inline/crates/nested/example/src"
cat >"$scratch/inline/Cargo.toml" <<'EOF'
[workspace]
members = ["crates/nested/example"]
resolver = "2"
EOF
cat >"$scratch/inline/crates/nested/example/Cargo.toml" <<'EOF'
[package]
name = "example"
version = "0.0.0"
edition = "2021"
EOF
cat >"$scratch/inline/crates/nested/example/src/lib.rs" <<'EOF'
#[test]
fn inline_test() {}

#[cfg(test)] mod tests {}
EOF
track_all "$scratch/inline"

if output=$(cd "$scratch/inline" && "$repo/.github/scripts/check-test-layout" 2>&1); then
  printf 'inline tests passed the layout policy\n' >&2
  exit 1
fi
[[ $output == *'crates/nested/example/src/lib.rs:1:#[test]'* ]]
[[ $output == *'crates/nested/example/src/lib.rs:4:#[cfg(test)] mod tests {}'* ]]

mkdir -p "$scratch/explicit/crates/nested/example/src" "$scratch/explicit/crates/nested/example/tests"
cat >"$scratch/explicit/Cargo.toml" <<'EOF'
[workspace]
members = ["crates/nested/example"]
resolver = "2"
EOF
printf 'pub fn example() {}\n' >"$scratch/explicit/crates/nested/example/src/lib.rs"
printf 'fn main() {}\n' >"$scratch/explicit/crates/nested/example/tests/explicit.rs"
cat >"$scratch/explicit/crates/nested/example/Cargo.toml" <<'EOF'
[package]
name = "example"
version = "0.0.0"
edition = "2021"

[[ test ]]
name = "explicit"
path = "tests/explicit.rs"
EOF
track_all "$scratch/explicit"

if output=$(cd "$scratch/explicit" && "$repo/.github/scripts/check-test-layout" 2>&1); then
  printf 'explicit test target passed the layout policy\n' >&2
  exit 1
fi
[[ $output == 'explicit test target is not allowlisted: crates/nested/example/Cargo.toml: explicit' ]]

mkdir -p "$scratch/fuzz/crates/example/src" "$scratch/fuzz/crates/example/fuzz/src" \
  "$scratch/fuzz/crates/example/fuzz/tests"
cat >"$scratch/fuzz/Cargo.toml" <<'EOF'
[workspace]
members = ["crates/example"]
exclude = ["crates/example/fuzz"]
resolver = "2"
EOF
cat >"$scratch/fuzz/crates/example/Cargo.toml" <<'EOF'
[package]
name = "example"
version = "0.0.0"
edition = "2021"
EOF
printf 'pub fn example() {}\n' >"$scratch/fuzz/crates/example/src/lib.rs"
cat >"$scratch/fuzz/crates/example/fuzz/Cargo.toml" <<'EOF'
[package]
name = "example-fuzz"
version = "0.0.0"
edition = "2021"
publish = false

[workspace]

[[test]]
name = "fuzz-explicit"
path = "tests/explicit.rs"
EOF
printf '#[cfg(test)] mod tests {}\n' >"$scratch/fuzz/crates/example/fuzz/src/lib.rs"
printf 'fn main() {}\n' >"$scratch/fuzz/crates/example/fuzz/tests/explicit.rs"
track_all "$scratch/fuzz"

if output=$(cd "$scratch/fuzz" && "$repo/.github/scripts/check-test-layout" 2>&1); then
  printf 'fuzz tests passed the layout policy\n' >&2
  exit 1
fi
[[ $output == *'explicit test target is not allowlisted: crates/example/fuzz/Cargo.toml: fuzz-explicit'* ]]
[[ $output == *'crates/example/fuzz/src/lib.rs:1:#[cfg(test)] mod tests {}'* ]]

mkdir -p "$scratch/root-tests/crates/example/src" "$scratch/root-tests/tests/frontend"
cat >"$scratch/root-tests/Cargo.toml" <<'EOF'
[workspace]
members = ["crates/example"]
resolver = "2"
EOF
cat >"$scratch/root-tests/crates/example/Cargo.toml" <<'EOF'
[package]
name = "example"
version = "0.0.0"
edition = "2021"
EOF
printf 'pub fn example() {}\n' >"$scratch/root-tests/crates/example/src/lib.rs"
printf '{}\n' >"$scratch/root-tests/tests/frontend/package.json"
track_all "$scratch/root-tests"

if output=$(cd "$scratch/root-tests" && "$repo/.github/scripts/check-test-layout" 2>&1); then
  printf 'root tests passed the layout policy\n' >&2
  exit 1
fi
[[ $output == 'root test path is forbidden: tests/frontend/package.json' ]]

mkdir -p "$scratch/generated/crates/example/src" \
  "$scratch/generated/crates/example/tests/frontend/test-results" \
  "$scratch/generated/crates/example/tests/frontend/node_modules" \
  "$scratch/generated/.tox/tests/test-results"
cat >"$scratch/generated/Cargo.toml" <<'EOF'
[workspace]
members = ["crates/example"]
resolver = "2"
EOF
cat >"$scratch/generated/crates/example/Cargo.toml" <<'EOF'
[package]
name = "example"
version = "0.0.0"
edition = "2021"
EOF
printf 'pub fn example() {}\n' >"$scratch/generated/crates/example/src/lib.rs"
printf '{}\n' >"$scratch/generated/crates/example/tests/frontend/.last-run.json"
printf '{}\n' >"$scratch/generated/crates/example/tests/frontend/node_modules/.package-lock.json"
printf '{}\n' >"$scratch/generated/crates/example/tests/frontend/test-results/results.json"
printf '{}\n' >"$scratch/generated/.tox/tests/test-results/.last-run.json"
printf 'node_modules/\n' >"$scratch/generated/.gitignore"
(cd "$scratch/generated" && git init -q && git add .gitignore Cargo.toml crates/example/Cargo.toml \
  crates/example/src/lib.rs crates/example/tests/frontend/.last-run.json \
  crates/example/tests/frontend/test-results)

if output=$(cd "$scratch/generated" && "$repo/.github/scripts/check-test-layout" 2>&1); then
  printf 'generated Playwright state passed the layout policy\n' >&2
  exit 1
fi
[[ $output == *'generated frontend artifact under tests: crates/example/tests/frontend/test-results'* ]]
[[ $output == *'generated frontend artifact under tests: crates/example/tests/frontend/.last-run.json'* ]]
[[ $output != *'node_modules'* ]]
[[ $output != *'.tox/tests/test-results'* ]]
