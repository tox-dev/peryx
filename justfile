set shell := ["bash", "-euo", "pipefail", "-c"]

project_tmp := justfile_directory() + "/.tox/tmp"
coverage_target_root := justfile_directory() + "/.tox/coverage-target"
native_coverage_target := env_var_or_default("CARGO_TARGET_DIR", justfile_directory() + "/target") + "/llvm-cov-target"
native_coverage_binary := native_coverage_target + "/debug/peryx" + if os_family() == "windows" { ".exe" } else { "" }
frontend_root := justfile_directory() + "/.tox/frontend"
tools_root := justfile_directory() + "/.tox/tools"
export PERYX_TEST_TMPDIR := project_tmp
export PLAYWRIGHT_BROWSERS_PATH := frontend_root + "/browsers"

# Run the default test suite.
default: test

# Create the project-owned temporary directory.
_project-temp:
    mkdir -p "{{ project_tmp }}"

# Verify that the Docker daemon is available.
_docker-ready:
    docker info >/dev/null

# Check CodSpeed benchmark selections.
_codspeed-target-contract:
    #!/usr/bin/env bash
    set -euo pipefail
    metadata="$(cargo metadata --no-deps --format-version 1)"
    jq -e '
      def benches($package):
        [.packages[] | select(.name == $package) | .targets[] | select(.kind == ["bench"])];
      (benches("peryx-ecosystem-oci") | length) == 4 and
      (benches("peryx-ecosystem-pypi") | map(select(."required-features" | length == 0)) | length) == 7 and
      (benches("peryx-ecosystem-pypi") | map(select(.name == "serve" or .name == "transform")) | length) == 2
    ' <<<"$metadata"
    just --dry-run codspeed-build peryx-ecosystem-oci all 2>&1 \
      | grep -F 'case "all" in'
    just --dry-run codspeed-build peryx-ecosystem-pypi parsing 2>&1 \
      | grep -F 'case "parsing" in'
    just --dry-run codspeed-build peryx-ecosystem-pypi serving 2>&1 \
      | grep -F 'case "serving" in'
    just --dry-run codspeed-run peryx-ecosystem-oci 2>&1 \
      | grep -F 'cargo codspeed run -m "simulation" --package "peryx-ecosystem-oci"'
    just --dry-run codspeed-run peryx-ecosystem-pypi 2>&1 \
      | grep -F 'cargo codspeed run -m "simulation" --package "peryx-ecosystem-pypi"'

# Check coverage target isolation.
_coverage-target-contract:
    CARGO_TARGET_DIR="{{ project_tmp }}/coverage-target-contract" just --dry-run coverage-frontend 2>&1 \
      | grep -F 'export CARGO_TARGET_DIR="{{ project_tmp }}/coverage-target-contract/frontend"'
    env -u CARGO_TARGET_DIR just --dry-run coverage-frontend 2>&1 \
      | grep -F 'export CARGO_TARGET_DIR="{{ coverage_target_root }}/frontend"'
    just --dry-run coverage-native 2>&1 \
      | grep -F 'PERYX_BIN="{{ native_coverage_binary }}"'

# Check that archived sanitizer tests receive the relocated Peryx binary.
_sanitizer-target-contract:
    just --dry-run sanitizer-run archive.tar.zst slice:1/8 2>&1 \
      | grep -F 'tar --extract --to-stdout --file "archive.tar.zst" target/nextest/binaries-metadata.json'
    just --dry-run sanitizer-run archive.tar.zst slice:1/8 2>&1 \
      | grep -F 'PERYX_BIN="$scratch/target/$binary"'
    just --dry-run sanitizer-run archive.tar.zst slice:1/8 2>&1 \
      | grep -F -- '--extract-to "$scratch"'

# Check mutation shard planning.
_mutation-shard-count-contract:
    test "$(just mutation-shard-count 255 256)" = 1
    test "$(just mutation-shard-count 8193 256)" = 33
    test "$(just mutation-shard-count 513 256)" = 3

# Check the zero-feature binary with declared CI tools.
_features-tool-contract:
    #!/usr/bin/env bash
    set -euo pipefail
    export PATH="$(dirname "$(command -v cargo)"):/usr/bin:/bin"
    if command -v rg >/dev/null; then
      echo 'ripgrep is present in the feature contract' >&2
      exit 1
    fi
    "{{ just_executable() }}" _zero-feature-binary

# Check Rust formatting.
format-check: _project-temp
    cargo fmt --all --check --

# Check every workspace target with all features.
check: _project-temp
    cargo check --workspace --all-targets --all-features

# Lint every workspace target with Clippy.
clippy: _project-temp
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# Check Rust formatting and lints.
lint-source: format-check clippy

# Check rustdoc, Markdown, and spelling.
lint-docs: _project-temp
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps
    prek run mdformat --all-files
    prek run codespell --all-files

# Check workflows and repository automation.
lint-automation: _project-temp _codspeed-target-contract _coverage-target-contract _mutation-shard-count-contract _sanitizer-target-contract _features-tool-contract
    SKIP=cargo-fmt,cargo-clippy,mdformat,codespell prek run --all-files

# Check dependency policy.
lint-deps: _project-temp
    cargo deny check

# Check committed PyPI snapshots.
snapshots: _project-temp
    cargo insta test --package peryx-ecosystem-pypi --lib --all-features \
      --unreferenced reject --test-runner nextest --nextest-profile ci

# Check workspace public API compatibility.
semver base="origin/main": _project-temp
    cargo semver-checks check-release --workspace --default-features --baseline-rev "{{ base }}"

# Check one deterministic shard of workspace public APIs.
semver-shard shard shards base="origin/main": _project-temp
    cargo metadata --no-deps --format-version 1 \
      | jq -r --argjson shard "{{ shard }}" --argjson shards "{{ shards }}" \
        '[.packages[] | select(.publish != []) | .name] | to_entries[] | select(.key % $shards == $shard) | .value' \
      | xargs -n 1 cargo semver-checks check-release --default-features --baseline-rev "{{ base }}" --package

# Check snapshots, public APIs, and the release plan.
lint-contracts base="origin/main": snapshots _coverage-target-contract
    just semver "{{ base }}"
    just release-plan

# Run every lint lane.
lint base="origin/main": _project-temp
    just lint-source
    just lint-docs
    just lint-automation
    just lint-deps
    just lint-contracts "{{ base }}"

# Install external test tools into the project cache.
test-deps: _project-temp
    PATH="{{ tools_root }}/bin:$PATH" UV_TOOL_BIN_DIR="{{ tools_root }}/bin" \
      UV_TOOL_DIR="{{ tools_root }}" uv tool install twine

# Run workspace tests, doctests, and benchmark harnesses.
test: test-deps
    PATH="{{ tools_root }}/bin:$PATH" cargo nextest run \
      --workspace --exclude peryx-storage --all-features --profile ci \
      -E 'not(test(e2e_live))'
    cargo nextest run --package peryx-storage --profile ci
    cargo test --workspace --all-features --doc
    just benchmark

# Run workspace benchmark harnesses as tests.
benchmark: _project-temp
    cargo test --workspace --all-features --bench '*' --no-fail-fast

# Run tests that cover platform-specific boundaries.
platform-test: _project-temp
    cargo check --workspace --all-targets --all-features
    cargo nextest run --package peryx --test cli_entrypoint --all-features --profile ci
    cargo nextest run --package peryx-upstream --all-features --profile ci
    cargo nextest run --package peryx-test-support --all-features --profile ci
    cargo nextest run --package peryx-storage --all-features --test integration \
      --profile ci -E 'test(/blob_backend/)'

# Run hermetic PyPI client boundary tests.
e2e: _project-temp
    PERYX_BIN="$(just _system-test-build composition-pypi)" PERYX_SINGLE_COMPOSITION=1 \
      cargo nextest run -p peryx-pypi-system-tests \
      --features e2e --test e2e -E 'not(test(e2e_live))'

# Run live PyPI client boundary tests.
e2e-live: test-deps
    PERYX_BIN="$(just _system-test-build composition-pypi)" PERYX_SINGLE_COMPOSITION=1 \
      PATH="{{ tools_root }}/bin:$PATH" cargo nextest run -p peryx-pypi-system-tests \
      --features e2e-live --test e2e -E 'test(e2e_live)'

# Run PyPI system tests without external-service cases.
pypi-system: _project-temp
    PERYX_BIN="$(just _system-test-build composition-pypi)" PERYX_SINGLE_COMPOSITION=1 \
      cargo nextest run -p peryx-pypi-system-tests --tests \
      -E 'not(binary(e2e)) & not(binary(availability)) & not(binary(s3_upload))'

# Run OCI system tests without availability cases.
oci-system: _project-temp
    PERYX_BIN="$(just _system-test-build composition-oci)" PERYX_SINGLE_COMPOSITION=1 \
      cargo nextest run -p peryx-oci-system-tests --tests -E 'not(binary(availability))'

# Run the PyPI S3 upload tests.
s3: _project-temp
    PERYX_BIN="$(just _system-test-build composition-pypi)" PERYX_SINGLE_COMPOSITION=1 \
      cargo nextest run -p peryx-pypi-system-tests --test s3_upload

# Run storage tests backed by S3 containers.
storage-s3: _project-temp _docker-ready
    cargo nextest run -p peryx-storage --features container-tests --test integration

# Run distributed availability tests.
availability: _project-temp
    cargo nextest run -p peryx --features availability-e2e --test availability --test cluster --test observability
    PERYX_BIN="$(just _system-test-build composition-pypi)" PERYX_SINGLE_COMPOSITION=1 \
      cargo nextest run -p peryx-pypi-system-tests --test availability
    PERYX_BIN="$(just _system-test-build composition-oci)" PERYX_SINGLE_COMPOSITION=1 \
      cargo nextest run -p peryx-oci-system-tests --test availability

# Run an availability simulation selection.
simulation filter="all()": _project-temp
    cargo nextest run -p peryx --features sim-campaign --test sim_campaign -E '{{ filter }}'

# Check every feature independently.
features: _project-temp
    cargo check --package peryx --no-default-features --lib
    just _zero-feature-binary
    cargo hack --workspace --exclude peryx --each-feature check --all-targets
    cargo hack --package peryx --each-feature --features composition-pypi check --all-targets
    cargo check --package peryx --no-default-features --features composition-oci --all-targets

# Check that the binary rejects an empty composition.
_zero-feature-binary:
    @if output="$(cargo check --package peryx --no-default-features --bin peryx 2>&1)"; then \
      echo 'zero-feature peryx binary compiled' >&2; exit 1; \
    fi; printf '%s\n' "$output" | grep -F 'the peryx binary requires at least one `composition-*` feature'

# Build the shipped server with one composition feature.
_system-test-build feature: _project-temp
    cargo build --package peryx --bin peryx --no-default-features --features "{{ feature }}" \
      --message-format json-render-diagnostics \
      | jq -er 'if .reason == "compiler-message" then (.message.rendered | stderr | empty) \
        elif .reason == "compiler-artifact" and .target.kind == ["bin"] and .target.name == "peryx" then .executable \
        else empty end'

# Check direct dependency lower bounds.
direct-minimum: _project-temp
    rm -rf .tox/direct-minimum
    rsync -a --exclude .git --exclude .tox --exclude target ./ .tox/direct-minimum/
    cargo +nightly update --manifest-path .tox/direct-minimum/Cargo.toml -Z direct-minimal-versions
    cargo +nightly check --manifest-path .tox/direct-minimum/Cargo.toml --workspace --all-targets
    rm -rf .tox/direct-minimum

# Interpret pure core crates with Miri.
miri: _project-temp
    TMPDIR="${RUNNER_TEMP:-/tmp}" cargo +nightly miri test --package peryx-core --lib --tests
    TMPDIR="${RUNNER_TEMP:-/tmp}" cargo +nightly miri test --package peryx-pql --lib --tests
    TMPDIR="${RUNNER_TEMP:-/tmp}" cargo +nightly miri test --package peryx-policy --lib --tests

# Check distributed runtime interleavings with Loom.
loom: _project-temp
    RUSTFLAGS="--cfg peryx_loom" cargo test --package peryx-ha-distributed --lib runtime_worker::loom_tests

# Run AddressSanitizer against a workspace partition.
sanitizer-address partition="slice:1/1": test-deps
    ASAN_OPTIONS=allow_addr2line=1 RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-Zsanitizer=address" \
      PATH="{{ tools_root }}/bin:$PATH" \
      cargo +nightly nextest run -Z build-std --workspace --target x86_64-unknown-linux-gnu \
      --features peryx/process-fixture --profile ci --build-jobs 1 --test-threads 1 \
      --partition "{{ partition }}" -E 'not(test(e2e_live))'

# Build the AddressSanitizer test archive.
sanitizer-archive archive: _project-temp
    RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-Zsanitizer=address" PATH="{{ tools_root }}/bin:$PATH" \
      cargo +nightly nextest archive -Z build-std --workspace --target x86_64-unknown-linux-gnu \
      --features peryx/process-fixture --profile ci --build-jobs 1 --archive-file "{{ archive }}"

# Run a partition from an AddressSanitizer archive.
sanitizer-run archive partition="slice:1/1": test-deps
    #!/usr/bin/env bash
    set -euo pipefail
    scratch=$(mktemp -d "{{ project_tmp }}/sanitizer.XXXXXX")
    trap 'rm -rf "$scratch"' EXIT
    binary=$(
      tar --extract --to-stdout --file "{{ archive }}" target/nextest/binaries-metadata.json \
        | jq -er '
            [."rust-build-meta"."non-test-binaries"[][]
              | select(.name == "peryx" and .kind == "bin-exe")
              | .path]
            | unique
            | if length == 1 then .[0] else error("archive must contain one Peryx server binary") end
          '
    )
    ASAN_OPTIONS=allow_addr2line=1 PERYX_BIN="$scratch/target/$binary" \
      PATH="{{ tools_root }}/bin:$PATH" cargo +nightly nextest run \
      --archive-file "{{ archive }}" --extract-to "$scratch" \
      --workspace-remap "{{ justfile_directory() }}" --profile ci --test-threads 1 \
      --partition "{{ partition }}" -E 'not(test(e2e_live))'

# Run one cargo-fuzz target.
fuzz package target seconds="60": _project-temp
    cd "crates/{{ package }}/fuzz" && cargo +nightly fuzz run \
      --target "$(rustc +nightly --print host-tuple)" "{{ target }}" -- -max_total_time="{{ seconds }}"

# Mutate one workspace shard.
mutation shard="0/1" in_place="false" jobs="2" baseline="run" timeout="500" sharding="slice": test-deps
    PATH="{{ tools_root }}/bin:$PATH" cargo mutants --workspace --all-features --test-tool nextest \
      --no-shuffle --shard "{{ shard }}" --sharding "{{ sharding }}" --output .tox/mutants \
      {{ if in_place == "true" { "--in-place" } else { "--jobs " + jobs } }} \
      --jobserver-tasks "{{ jobs }}" --baseline "{{ baseline }}" \
      --timeout "{{ timeout }}" --build-timeout "{{ timeout }}" \
      -- --profile mutation -E 'not(test(e2e_live))'

# Run one mutation shard with Linux resource telemetry.
mutation-observed shard="0/1" in_place="false" jobs="2" baseline="run" timeout="500" sharding="slice":
    #!/usr/bin/env bash
    set -uo pipefail
    cgroup_root="/sys/fs/cgroup$(awk -F: '$1 == "0" { print $3 }' /proc/self/cgroup 2>/dev/null)"
    if [[ ! -r "$cgroup_root/cgroup.controllers" ]]; then
      printf 'mutation resource telemetry requires Linux cgroup v2\n' >&2
      exit 2
    fi
    sample() {
      local metric pressure progress
      if [[ -r .tox/mutants/outcomes.json ]]; then
        progress="$(jq -c '{
          completed: (.outcomes | length),
          last: (.outcomes[-1].scenario.Mutant.name //
            (if (.outcomes | length) > 0 then (.outcomes[-1].scenario | tostring) else null end))
        }' .tox/mutants/outcomes.json 2>/dev/null || printf '{"completed":null,"last":null}')"
      else
        progress='{"completed":0,"last":null}'
      fi
      printf 'mutation-resource timestamp=%s progress=%s' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$progress"
      for metric in memory.current memory.peak pids.current; do
        if [[ -r "$cgroup_root/$metric" ]]; then
          printf ' %s=%s' "$metric" "$(<"$cgroup_root/$metric")"
        fi
      done
      for metric in memory.events pids.events; do
        if [[ -r "$cgroup_root/$metric" ]]; then
          printf ' %s=%s' "$metric" "$(awk '{printf "%s%s=%s", NR == 1 ? "" : ",", $1, $2}' "$cgroup_root/$metric")"
        fi
      done
      for pressure in cpu memory io; do
        if [[ -r "/proc/pressure/$pressure" ]]; then
          printf ' %s.pressure=%s' "$pressure" "$(paste -sd ';' "/proc/pressure/$pressure")"
        fi
      done
      printf '\n'
    }
    sample
    while sleep 60; do sample; done &
    monitor_pid=$!
    trap 'kill "$monitor_pid" 2>/dev/null || :; wait "$monitor_pid" 2>/dev/null || :' EXIT
    just mutation "{{ shard }}" "{{ in_place }}" "{{ jobs }}" "{{ baseline }}" "{{ timeout }}" "{{ sharding }}"
    mutation_status=$?
    sample
    printf 'mutation-exit status=%d\n' "$mutation_status"
    exit "$mutation_status"

# Run the mutation baseline suite.
mutation-baseline: test-deps
    INSTA_UPDATE=no INSTA_FORCE_PASS=0 PATH="{{ tools_root }}/bin:$PATH" cargo nextest run --verbose \
      --workspace --all-features --profile ci -E 'not(test(e2e_live))'

# Build the mutation baseline test archive.
mutation-baseline-archive archive: _project-temp
    cargo nextest archive --workspace --all-features --profile ci --archive-file "{{ archive }}"

# Run a partition from the mutation baseline archive.
mutation-baseline-run archive partition="slice:1/1": test-deps
    INSTA_UPDATE=no INSTA_FORCE_PASS=0 PATH="{{ tools_root }}/bin:$PATH" \
      cargo nextest run --archive-file "{{ archive }}" --workspace-remap "{{ justfile_directory() }}" \
      --profile ci --partition "{{ partition }}" -E 'not(test(e2e_live))'

# Count workspace mutation candidates.
mutation-count: _project-temp
    cargo mutants --list --workspace --all-features | wc -l

# Calculate the shard count for a mutant total and per-shard target.
mutation-shard-count mutants target:
    #!/usr/bin/env bash
    set -euo pipefail
    mutants={{ quote(mutants) }}
    target={{ quote(target) }}
    if ! [[ "$mutants" =~ ^[1-9][0-9]*$ && "$target" =~ ^[1-9][0-9]*$ ]]; then
      printf 'mutants and target must be positive integers\n' >&2
      exit 1
    fi
    printf '%d\n' "$(( (mutants + target - 1) / target ))"

# Install browser-test dependencies for the shared and owner suites.
frontend-deps: _project-temp
    npm --prefix crates/peryx-web/tests/frontend ci
    npm --prefix crates/peryx-ecosystem-pypi/tests/frontend ci
    npm --prefix crates/peryx-ecosystem-oci/tests/frontend ci

# Install Chromium and optional host dependencies for browser tests.
frontend-browser-deps *args: _project-temp
    npm --prefix crates/peryx-web/tests/frontend exec -- playwright install {{ args }} chromium

# Run the shared and owner browser suites against an existing build.
frontend-test: _project-temp
    npm --prefix crates/peryx-web/tests/frontend test
    npm --prefix crates/peryx-ecosystem-pypi/tests/frontend test
    npm --prefix crates/peryx-ecosystem-oci/tests/frontend test

# Print tool versions used by local and container validation.
versions: _project-temp
    rustc --version
    cargo --version
    cargo nextest --version
    cargo llvm-cov --version
    just --version
    node --version
    npm --version

# Refresh locked mise tool versions and checksums.
mise-lock:
    mise lock --bump

# Build and test the browser application.
frontend: frontend-deps _project-temp
    just frontend-browser-deps
    cargo leptos build
    just frontend-test

# Serve the documentation source.
site-dev: _project-temp
    zola --root site serve --interface 127.0.0.1

# Write the workspace dependency graph as Mermaid source.
crate-dependency-diagram output="site/diagrams/crate-dependencies.mmd": _project-temp
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{ output }}" in
      site/diagrams/crate-dependencies.mmd|.tox/site/crate-dependencies.mmd) ;;
      *) printf 'unsupported diagram output: %s\n' "{{ output }}" >&2; exit 1 ;;
    esac
    mkdir -p "$(dirname "{{ output }}")"
    {
    printf '%s\n' '---' 'config:' '  layout: elk' '  elk:' '    mergeEdges: true' '---'
    printf 'flowchart TD\n'
    # LC_ALL=C on the sort below: a UTF-8 collation orders `[` and `_` differently, so a
    # developer machine regenerates a file that differs from CI's and the staleness check
    # in `diagrams` fails against a correct committed diagram.
    cargo metadata --format-version 1 --no-deps | jq -r '
      (.packages | map(.name) | unique) as $workspace
      | ($workspace[] | "  \(gsub("-"; "_"))[\(. | @json)]"),
        (.packages[] | .name as $source | .dependencies[]
          | select(.kind == null and .path != null)
          | select(.name as $dependency | $workspace | index($dependency))
          | "  \($source | gsub("-"; "_")) --> \(.name | gsub("-"; "_"))")
    ' | LC_ALL=C sort -u
    printf '  class peryx accent\n'
    printf '  class peryx_ecosystem_oci,peryx_ecosystem_pypi good\n'
    printf '  class peryx_oci_system_tests,peryx_pypi_system_tests warn\n'
    } > "{{ output }}"

# Pre-render every Mermaid diagram for light and dark themes.
render-diagrams output="site/static/diagrams": _project-temp
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{ output }}" in
      site/static/diagrams|.tox/site/diagrams) ;;
      *) printf 'unsupported diagram output: %s\n' "{{ output }}" >&2; exit 1 ;;
    esac
    npm --prefix site ci
    rm -rf "{{ output }}"
    mkdir -p "{{ output }}"
    (
      cd site
      node --input-type=module - "../{{ output }}" <<'NODE'
    import { readdir, readFile } from "node:fs/promises";
    import { basename, join } from "node:path";
    import { run } from "@mermaid-js/mermaid-cli";
    import puppeteer from "puppeteer";

    const output = process.argv[2];
    const sources = (await readdir("diagrams", { withFileTypes: true }))
      .filter((entry) => entry.isFile() && entry.name.endsWith(".mmd"))
      .map((entry) => join("diagrams", entry.name))
      .sort();
    const themes = await Promise.all(
      ["light", "dark"].map(async (name) => ({
        name,
        config: JSON.parse(await readFile(`diagrams/${name}.json`, "utf8")),
      })),
    );
    const browser = await puppeteer.launch({ headless: "shell" });
    try {
      for (const source of sources) {
        const name = basename(source, ".mmd");
        for (const theme of themes) {
          await run(source, `${output}/${name}-${theme.name}.svg.tmp.svg`, {
            browser,
            outputFormat: "svg",
            parseMMDOptions: {
              backgroundColor: "transparent",
              iconPacks: [],
              iconPacksNamesAndUrls: [],
              mermaidConfig: { theme: "default", ...theme.config },
              svgId: `peryx-${name}-${theme.name}`,
              viewport: { width: 800, height: 600, deviceScaleFactor: 1 },
            },
            quiet: true,
          });
        }
      }
    } finally {
      await browser.close();
    }
    NODE
    )
    for source in site/diagrams/*.mmd; do
      name=$(basename "$source" .mmd)
      for theme in light dark; do
        rendered="{{ output }}/$name-$theme.svg"
        digest=$(shasum -a 256 "$source" "site/diagrams/$theme.json" site/package-lock.json | \
          shasum -a 256 | cut -d ' ' -f 1)
        { printf '<!-- peryx-mermaid-input-sha256=%s -->\n' "$digest"; awk '1' "$rendered.tmp.svg"; } > "$rendered"
        rm "$rendered.tmp.svg"
      done
    done

# Check every pre-rendered Mermaid diagram against its source.
diagrams: _project-temp
    #!/usr/bin/env bash
    set -euo pipefail
    just crate-dependency-diagram .tox/site/crate-dependencies.mmd
    cmp site/diagrams/crate-dependencies.mmd .tox/site/crate-dependencies.mmd || \
      (printf 'crate dependency diagram is stale; run just crate-dependency-diagram\n' >&2; exit 1)
    just render-diagrams .tox/site/diagrams
    diff <(find site/static/diagrams -maxdepth 1 -type f -name '*.svg' -exec basename {} \; | sort) \
      <(find .tox/site/diagrams -maxdepth 1 -type f -name '*.svg' -exec basename {} \; | sort)
    for rendered in .tox/site/diagrams/*.svg; do
      committed="site/static/diagrams/$(basename "$rendered")"
      cmp <(sed -n '1p' "$committed") <(sed -n '1p' "$rendered") || \
        (printf '%s is stale; run just render-diagrams\n' "$committed" >&2; exit 1)
    done

# Build and validate the documentation site.
docs: diagrams
    zola --root site check --skip-external-links
    zola --root site build --force --output-dir "{{ justfile_directory() }}/.tox/site/public"
    cargo run --quiet --package peryx --bin peryx -- openapi > .tox/site/public/openapi.json
    npm --prefix site exec -- pagefind --site "{{ justfile_directory() }}/.tox/site/public" \
      --include-characters "_./-"

# Check external documentation links with Zola's checker.
site-links: _project-temp
    zola --root site check

# Build the documentation site.
site: docs

# Build the documentation site for Read the Docs.
site-readthedocs:
    : "${READTHEDOCS_CANONICAL_URL:?}"
    : "${READTHEDOCS_OUTPUT:?}"
    mkdir -p "$READTHEDOCS_OUTPUT/html"
    zola --root site build --base-url "$READTHEDOCS_CANONICAL_URL" --force \
      --output-dir "$READTHEDOCS_OUTPUT/html"
    CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 \
      cargo run --quiet --package peryx --bin peryx -- openapi > "$READTHEDOCS_OUTPUT/html/openapi.json"
    npm --prefix site exec -- pagefind --site "$READTHEDOCS_OUTPUT/html" \
      --include-characters "_./-"

# Validate the cargo-dist release plan.
release-plan: _project-temp
    dist plan --output-format=json > /dev/null

# Run the OCI distribution-spec conformance suite.
conformance suite binary="": _project-temp
    #!/usr/bin/env bash
    set -euo pipefail
    suite="{{ suite }}"
    mkdir -p "$(dirname "$suite")"
    suite="$(cd "$(dirname "$suite")" && pwd -P)/$(basename "$suite")"
    cargo build --bin peryx
    peryx="{{ binary }}"
    target=$(cargo metadata --no-deps --format-version 1 | jq -r .target_directory)
    peryx="${peryx:-$target/debug/peryx}"
    scratch=$(mktemp -d "{{ project_tmp }}/conformance.XXXXXX")
    mkdir "$scratch/tmp" "$scratch/go-build" "$scratch/go-mod"
    export TMPDIR="$scratch/tmp" TMP="$scratch/tmp" TEMP="$scratch/tmp"
    export GOCACHE="$scratch/go-build" GOMODCACHE="$scratch/go-mod"
    server_pid=
    cleanup() {
      status=$?
      trap - EXIT
      if [[ -n $server_pid ]] && kill -0 "$server_pid" 2>/dev/null; then
        kill "$server_pid"
        wait "$server_pid" || true
      fi
      cleanup_status=0
      go clean -modcache || cleanup_status=$?
      rm -rf "$scratch" || cleanup_status=$?
      ((status != 0)) && exit "$status"
      exit "$cleanup_status"
    }
    trap cleanup EXIT
    git -C "$scratch" init --quiet checkout
    git -C "$scratch/checkout" remote add origin https://github.com/opencontainers/distribution-spec
    git -C "$scratch/checkout" fetch --quiet --depth=1 origin fcfba1ec55526073f48b2f6d4e3d7eef410ddcbc
    git -C "$scratch/checkout" checkout --quiet --detach FETCH_HEAD
    go -C "$scratch/checkout/conformance" build -o "$suite" .
    cat >"$scratch/peryx.toml" <<EOF
    host = "127.0.0.1"
    port = 0
    data_dir = "$scratch/data"

    [log]
    format = "json"

    [[index]]
    name = "store"
    route = "store"
    ecosystem = "oci"
    hosted = true

    [[index.access_token]]
    name = "uploader"
    secret = "conformance"
    actions = ["write", "delete"]
    EOF
    "$peryx" serve --config "$scratch/peryx.toml" >"$scratch/server.log" 2>&1 &
    server_pid=$!
    port=
    for _ in {1..150}; do
      port=$(jq -r 'select(.fields.message == "peryx listening") | .fields.addr |
        capture(":(?<port>[0-9]+)$").port' "$scratch/server.log" 2>/dev/null | head -1)
      if [[ -n $port ]]; then
        break
      fi
      kill -0 "$server_pid" 2>/dev/null || break
      sleep 0.2
    done
    if [[ -z $port ]] || ! curl --fail --silent "http://127.0.0.1:$port/v2/" >/dev/null; then
      cat "$scratch/server.log" >&2
      exit 1
    fi
    (cd "$scratch" && OCI_REGISTRY="127.0.0.1:$port" OCI_TLS=disabled \
      OCI_REPO1=store/conformance OCI_REPO2=store/crossmount OCI_USERNAME=_ \
      OCI_PASSWORD=conformance OCI_DATA_SHA512=false "$suite")

# Build one package selection's CodSpeed benchmarks.
codspeed-build package suite mode="simulation": _project-temp
    #!/usr/bin/env bash
    set -euo pipefail
    flags=()
    case "{{ suite }}" in
      parsing) flags=(--no-default-features) ;;
      serving) flags=(--features bench --bench serve --bench transform) ;;
    esac
    cargo codspeed build --locked -m "{{ mode }}" --package "{{ package }}" "${flags[@]}"

# Run one package's built CodSpeed benchmarks.
codspeed-run package mode="simulation": _project-temp
    cargo codspeed run -m "{{ mode }}" --package "{{ package }}"

# Build and run one package's CodSpeed benchmarks locally.
codspeed package mode="simulation": _project-temp
    suites=(all); \
    if [[ "{{ package }}" == peryx-ecosystem-pypi ]]; then suites=(parsing serving); fi; \
    for suite in "${suites[@]}"; do \
      just codspeed-build "{{ package }}" "$suite" "{{ mode }}"; \
      just codspeed-run "{{ package }}" "{{ mode }}"; \
    done

# Build a local Python wheel.
package-wheel +args: _project-temp
    maturin build --release --locked --out dist {{ args }}

# Build a local source distribution.
package-sdist output="dist": _project-temp
    maturin sdist --out "{{ output }}"

# Measure native Rust coverage.
coverage-native output=".tox/coverage/native.lcov": test-deps _docker-ready
    mkdir -p "$(dirname "{{ output }}")"
    cargo llvm-cov clean --workspace
    cargo llvm-cov --workspace --all-features --bench '*' --no-report
    PERYX_BIN="{{ native_coverage_binary }}" \
      PATH="{{ tools_root }}/bin:$PATH" cargo llvm-cov nextest --workspace \
      --all-features --profile ci --lib --bins --tests --examples \
      -E 'not(test(e2e_live))' --no-report
    cargo llvm-cov report --no-default-ignore-filename-regex \
      --ignore-filename-regex '/(\.cargo/(registry|git)|\.rustup/toolchains|rustc/[0-9a-f]+)/' \
      --fail-uncovered-lines 0 --show-missing-lines --lcov --output-path "{{ output }}"

# Measure native and Wasm frontend coverage.
coverage-frontend native_output=".tox/coverage/frontend-native.lcov" wasm_output=".tox/coverage/frontend-wasm.lcov" merged_output=".tox/coverage/frontend.lcov": _project-temp
    #!/usr/bin/env bash
    set -euo pipefail
    native_output="{{ native_output }}"
    wasm_output="{{ wasm_output }}"
    merged_output="{{ merged_output }}"
    mkdir -p "$(dirname "$native_output")" "$(dirname "$wasm_output")" "$(dirname "$merged_output")"
    rm -f "$native_output" "$wasm_output" "$merged_output"
    scratch=$(mktemp -d "{{ project_tmp }}/coverage-frontend.XXXXXX")
    trap 'rm -rf "$scratch"' EXIT
    commit_date=$(rustc -vV | awk '/^commit-date:/ { print $2 }')
    toolchain="nightly-$commit_date"
    rustup toolchain install "$toolchain" --profile minimal --component llvm-tools-preview \
      --target wasm32-unknown-unknown
    stable_llvm=$(rustc -vV | awk '/^LLVM version:/ { split($3, version, "."); print version[1] }')
    nightly_llvm=$(rustc +"$toolchain" -vV | awk '/^LLVM version:/ { split($3, version, "."); print version[1] }')
    if [[ $stable_llvm != "$nightly_llvm" ]]; then
      printf 'stable LLVM %s does not match %s LLVM %s\n' "$stable_llvm" "$toolchain" "$nightly_llvm" >&2
      exit 1
    fi
    if [[ -n ${CLANG:-} ]]; then
      clang=$(command -v "$CLANG")
    elif clang=$(command -v "clang-$nightly_llvm" 2>/dev/null); then
      :
    elif command -v brew >/dev/null; then
      clang="$(brew --prefix llvm)/bin/clang"
    else
      printf 'clang %s is required for Wasm coverage\n' "$nightly_llvm" >&2
      exit 1
    fi
    clang_llvm=$($clang --version | sed -E -n '1s/.*version ([0-9]+).*/\1/p')
    if [[ $clang_llvm != "$nightly_llvm" ]]; then
      printf 'clang LLVM %s does not match %s LLVM %s\n' "$clang_llvm" "$toolchain" "$nightly_llvm" >&2
      exit 1
    fi
    export PATH="$(dirname "$clang"):$PATH"
    sysroot=$(rustc +"$toolchain" --print sysroot)
    host=$(rustc +"$toolchain" -vV | awk '/^host:/ { print $2 }')
    export LLVM_COV="$sysroot/lib/rustlib/$host/bin/llvm-cov"
    export LLVM_PROFDATA="$sysroot/lib/rustlib/$host/bin/llvm-profdata"
    export CARGO_TARGET_DIR="{{ env_var_or_default("CARGO_TARGET_DIR", coverage_target_root) }}/frontend"
    export CARGO_LLVM_COV_BUILD_DIR="$CARGO_TARGET_DIR"
    export CARGO_LLVM_COV_TARGET_DIR="$CARGO_TARGET_DIR"
    eval "$(cargo llvm-cov show-env --sh --no-cfg-coverage)"
    cargo llvm-cov clean --workspace
    cargo nextest run --package peryx-web --all-features --lib
    cargo llvm-cov report --no-default-ignore-filename-regex --lcov \
      --output-path "$scratch/native-unit.lcov"
    export RUSTUP_TOOLCHAIN="$toolchain"
    export CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS="-Zno-profiler-runtime --cfg=wasm_bindgen_unstable_test_coverage"
    unset RUSTFLAGS
    target=$(cargo metadata --no-deps --format-version 1 | jq -r .target_directory)
    wasm="{{ justfile_directory() }}/.tox/frontend/ui/pkg/peryx_web.wasm"
    mkdir -p "$CARGO_TARGET_DIR" "$scratch/profiles"
    find "$CARGO_TARGET_DIR" -maxdepth 1 -name 'peryx-frontend-*.profraw' -delete
    export LLVM_PROFILE_FILE="$CARGO_TARGET_DIR/peryx-frontend-%p-%10m.profraw"
    cargo leptos build --lib-features wasm-coverage --wasm-debug
    PERYX_FRONTEND_BINARY="$target/debug/peryx" PERYX_WASM_PROFRAW="$scratch/profiles" just frontend-test
    cargo llvm-cov report --no-default-ignore-filename-regex --lcov \
      --output-path "$scratch/native-browser.lcov"
    lcov --quiet --add-tracefile "$scratch/native-unit.lcov" \
      --add-tracefile "$scratch/native-browser.lcov" --output-file "$scratch/native.lcov"
    lcov --quiet --extract "$scratch/native.lcov" "{{ justfile_directory() }}/crates/peryx-web/src/*" \
      --output-file "$native_output"
    if [[ ! -s $wasm ]]; then
      printf 'frontend Wasm binary not found at %s\n' "$wasm" >&2
      exit 1
    fi
    "$LLVM_PROFDATA" merge -sparse "$scratch"/profiles/*.profraw -o "$scratch/wasm.profdata"
    "$LLVM_COV" export --format=lcov --instr-profile "$scratch/wasm.profdata" --object "$wasm" \
      --sources "{{ justfile_directory() }}/crates/peryx-web/src" >"$wasm_output"
    lcov --quiet --add-tracefile "$native_output" --add-tracefile "$wasm_output" \
      --output-file "$merged_output"
    lcov --summary "$merged_output" --fail-under-lines 100

# Measure native and frontend coverage.
coverage output=".tox/coverage": _project-temp
    just coverage-native "{{ output }}/native.lcov"
    just frontend-deps
    just coverage-frontend "{{ output }}/frontend-native.lcov" \
      "{{ output }}/frontend-wasm.lcov" "{{ output }}/frontend.lcov"

# Remove local Rust coverage build artifacts and locks.
coverage-clean:
    cargo llvm-cov clean --workspace
    rm -rf .tox/coverage .tox/coverage-sessions

# Remove transient project-owned artifacts.
clean: coverage-clean
    rm -rf .tox/bench/scratch .tox/conformance.test .tox/docker/tmp .tox/frontend .tox/site .tox/tmp \
      crates/peryx-ecosystem-oci/tests/frontend/blob-report \
      crates/peryx-ecosystem-oci/tests/frontend/playwright-report \
      crates/peryx-ecosystem-oci/tests/frontend/test-results \
      crates/peryx-ecosystem-pypi/tests/frontend/blob-report \
      crates/peryx-ecosystem-pypi/tests/frontend/playwright-report \
      crates/peryx-ecosystem-pypi/tests/frontend/test-results \
      crates/peryx-web/tests/frontend/blob-report crates/peryx-web/tests/frontend/playwright-report \
      crates/peryx-web/tests/frontend/test-results target-browser ui

# Remove project-owned artifacts, including reusable build state.
clean-all:
    cargo clean
    rm -rf .tox crates/peryx-ecosystem-oci/tests/frontend/node_modules \
      crates/peryx-ecosystem-pypi/tests/frontend/node_modules \
      crates/peryx-web/tests/frontend/node_modules crates/peryx-web/dist site/node_modules

# Run repository hooks against all files.
pre-commit: _project-temp
    prek run --all-files

# Run the complete CI suite.
ci: all

# Run lint, coverage, and documentation checks.
all: lint coverage docs
