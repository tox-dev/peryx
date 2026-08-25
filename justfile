set shell := ["bash", "-euo", "pipefail", "-c"]

project_tmp := justfile_directory() + "/.tox/tmp"
frontend_root := justfile_directory() + "/.tox/frontend"
tools_root := justfile_directory() + "/.tox/tools"
export PERYX_TEST_TMPDIR := project_tmp
export PLAYWRIGHT_BROWSERS_PATH := frontend_root + "/browsers"

default: test

_project-temp:
    mkdir -p "{{ project_tmp }}"

_docker-ready:
    docker info >/dev/null

format-check: _project-temp
    cargo fmt --all --check --

check: _project-temp
    cargo check --workspace --all-targets --all-features

clippy: _project-temp
    cargo clippy --workspace --all-targets --all-features -- -D warnings

lint-source: format-check clippy

lint-docs: _project-temp
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps
    prek run mdformat --all-files
    prek run codespell --all-files

lint-automation: _project-temp
    SKIP=cargo-fmt,cargo-clippy,mdformat,codespell prek run --all-files

lint-deps: _project-temp
    cargo deny check

snapshots: _project-temp
    cargo insta test --package peryx-ecosystem-pypi --lib --all-features \
      --unreferenced reject --test-runner nextest --nextest-profile ci

publishable-packages: _project-temp
    cargo metadata --no-deps --format-version 1 | jq -c '[.packages[] | select(.publish != []) | .name]'

semver base="origin/main": _project-temp
    cargo semver-checks check-release --workspace --default-features --baseline-rev "{{ base }}"

semver-package package base="origin/main": _project-temp
    cargo semver-checks check-release --package "{{ package }}" \
      --default-features --baseline-rev "{{ base }}"

lint-contracts base="origin/main": snapshots
    just semver "{{ base }}"
    just release-plan

lint base="origin/main": _project-temp
    just lint-source
    just lint-docs
    just lint-automation
    just lint-deps
    just lint-contracts "{{ base }}"

test-deps: _project-temp
    PATH="{{ tools_root }}/bin:$PATH" UV_TOOL_BIN_DIR="{{ tools_root }}/bin" \
      UV_TOOL_DIR="{{ tools_root }}" uv tool install twine

test: test-deps
    PATH="{{ tools_root }}/bin:$PATH" cargo nextest run \
      --workspace --exclude peryx-storage --all-features --profile ci \
      -E 'not(test(e2e_live))'
    cargo nextest run --package peryx-storage --profile ci
    cargo test --workspace --all-features --doc
    just benchmark

benchmark: _project-temp
    cargo test --workspace --all-features --bench '*' --no-fail-fast

platform-test: _project-temp
    cargo check --workspace --all-targets --all-features
    cargo nextest run --package peryx --test cli_entrypoint --all-features --profile ci
    cargo nextest run --package peryx-upstream --all-features --profile ci
    cargo nextest run --package peryx-test-support --all-features --profile ci
    cargo nextest run --package peryx-storage --all-features --test integration \
      --profile ci -E 'test(/blob_backend/)'

e2e: _project-temp
    cargo nextest run -p peryx-pypi-system-tests --features e2e --test e2e -E 'not(test(e2e_live))'

e2e-live: test-deps
    PATH="{{ tools_root }}/bin:$PATH" cargo nextest run -p peryx-pypi-system-tests \
      --features e2e-live --test e2e -E 'test(e2e_live)'

pypi-system: _project-temp
    cargo nextest run -p peryx-pypi-system-tests --tests \
      -E 'not(binary(e2e)) & not(binary(availability)) & not(binary(s3_upload))'

oci-system: _project-temp
    cargo nextest run -p peryx-oci-system-tests --tests -E 'not(binary(availability))'

s3: _project-temp
    cargo nextest run -p peryx-pypi-system-tests --test s3_upload

storage-s3: _project-temp _docker-ready
    cargo nextest run -p peryx-storage --features container-tests --test integration

availability: _project-temp
    cargo nextest run -p peryx --features availability-e2e --test availability --test cluster --test observability
    cargo nextest run -p peryx-pypi-system-tests --test availability
    cargo nextest run -p peryx-oci-system-tests --test availability

simulation filter="all()": _project-temp
    cargo nextest run -p peryx --features sim-campaign --test sim_campaign -E '{{ filter }}'

features: _project-temp
    cargo hack --workspace --each-feature check --all-targets

direct-minimum: _project-temp
    rm -rf .tox/direct-minimum
    rsync -a --exclude .git --exclude .tox --exclude target ./ .tox/direct-minimum/
    cargo +nightly update --manifest-path .tox/direct-minimum/Cargo.toml -Z direct-minimal-versions
    cargo +nightly check --manifest-path .tox/direct-minimum/Cargo.toml --workspace --all-targets
    rm -rf .tox/direct-minimum

miri: _project-temp
    TMPDIR="${RUNNER_TEMP:-/tmp}" cargo +nightly miri test --package peryx-core --lib --tests
    TMPDIR="${RUNNER_TEMP:-/tmp}" cargo +nightly miri test --package peryx-pql --lib --tests
    TMPDIR="${RUNNER_TEMP:-/tmp}" cargo +nightly miri test --package peryx-policy --lib --tests

loom: _project-temp
    RUSTFLAGS="--cfg peryx_loom" cargo test --package peryx-ha-distributed --lib runtime_worker::loom_tests

sanitizer-address partition="slice:1/1":
    just sanitizer address "{{ partition }}"

sanitizer-thread partition="slice:1/1":
    just sanitizer thread "{{ partition }}"

# TSan needs the standard library's synchronization operations instrumented.
# https://doc.rust-lang.org/beta/unstable-book/compiler-flags/sanitizer.html#threadsanitizer
sanitizer sanitizer partition="slice:1/1": test-deps
    ASAN_OPTIONS=allow_addr2line=1 TSAN_OPTIONS=allow_addr2line=1:halt_on_error=1:io_sync=2 \
      RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-Zsanitizer={{ sanitizer }}" PATH="{{ tools_root }}/bin:$PATH" \
      cargo +nightly nextest run -Z build-std --workspace --target x86_64-unknown-linux-gnu \
      --profile ci --build-jobs 1 --test-threads 1 --partition "{{ partition }}" -E 'not(test(e2e_live))'

sanitizer-archive sanitizer archive: _project-temp
    RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-Zsanitizer={{ sanitizer }}" PATH="{{ tools_root }}/bin:$PATH" \
      cargo +nightly nextest archive -Z build-std --workspace --target x86_64-unknown-linux-gnu \
      --profile ci --build-jobs 1 --archive-file "{{ archive }}"

sanitizer-run archive partition="slice:1/1": test-deps
    ASAN_OPTIONS=allow_addr2line=1 TSAN_OPTIONS=allow_addr2line=1:halt_on_error=1:io_sync=2 \
      PATH="{{ tools_root }}/bin:$PATH" cargo +nightly nextest run --archive-file "{{ archive }}" \
      --workspace-remap "{{ justfile_directory() }}" --profile ci --test-threads 1 \
      --partition "{{ partition }}" -E 'not(test(e2e_live))'

fuzz package target seconds="60": _project-temp
    cd "crates/{{ package }}/fuzz" && cargo +nightly fuzz run \
      --target "$(rustc +nightly --print host-tuple)" "{{ target }}" -- -max_total_time="{{ seconds }}"

mutation shard="0/1" in_place="false" jobs="2" baseline="run" timeout="500": test-deps
    PATH="{{ tools_root }}/bin:$PATH" cargo mutants --workspace --all-features --test-tool nextest \
      --no-shuffle --shard "{{ shard }}" --output .tox/mutants \
      {{ if in_place == "true" { "--in-place" } else { "--jobs " + jobs } }} \
      --jobserver-tasks "{{ jobs }}" --baseline "{{ baseline }}" \
      --timeout "{{ timeout }}" --build-timeout "{{ timeout }}" \
      -- --profile ci -E 'not(test(e2e_live))'

mutation-baseline: test-deps
    INSTA_UPDATE=no INSTA_FORCE_PASS=0 PATH="{{ tools_root }}/bin:$PATH" cargo nextest run --verbose \
      --workspace --all-features --profile ci -E 'not(test(e2e_live))'

mutation-baseline-archive archive: _project-temp
    cargo nextest archive --workspace --all-features --profile ci --archive-file "{{ archive }}"

mutation-baseline-run archive partition="slice:1/1": test-deps
    INSTA_UPDATE=no INSTA_FORCE_PASS=0 PATH="{{ tools_root }}/bin:$PATH" \
      cargo nextest run --archive-file "{{ archive }}" --workspace-remap "{{ justfile_directory() }}" \
      --profile ci --partition "{{ partition }}" -E 'not(test(e2e_live))'

mutation-count: _project-temp
    cargo mutants --list --workspace --all-features | wc -l

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

# Build and validate the documentation site.
docs: _project-temp
    zola --root site check --skip-external-links
    zola --root site build --force --output-dir "{{ justfile_directory() }}/.tox/site/public"
    cargo run --quiet --package peryx --bin peryx -- openapi > .tox/site/public/openapi.json
    npm --prefix site ci
    npm --prefix site exec -- pagefind --site "{{ justfile_directory() }}/.tox/site/public" \
      --include-characters "_./-"

# Check external documentation links with Zola's checker.
site-links: _project-temp
    zola --root site check

site: docs

# Build the documentation site for Read the Docs.
site-readthedocs: _project-temp
    : "${READTHEDOCS_CANONICAL_URL:?}"
    : "${READTHEDOCS_OUTPUT:?}"
    mkdir -p "$READTHEDOCS_OUTPUT/html"
    zola --root site build --base-url "$READTHEDOCS_CANONICAL_URL" --force \
      --output-dir "$READTHEDOCS_OUTPUT/html"
    CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 \
      cargo run --quiet --package peryx --bin peryx -- openapi > "$READTHEDOCS_OUTPUT/html/openapi.json"
    npm --prefix site ci
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

# Build one package's CodSpeed benchmarks.
codspeed-build package mode="simulation": _project-temp
    cargo codspeed build --locked -m "{{ mode }}" --package "{{ package }}"

# Run one package's built CodSpeed benchmarks.
codspeed-run package: _project-temp
    cargo codspeed run --package "{{ package }}"

# Build and run one package's CodSpeed benchmarks locally.
codspeed package mode="simulation": _project-temp
    just codspeed-build "{{ package }}" "{{ mode }}"
    codspeed run --mode "{{ mode }}" -- just codspeed-run "{{ package }}"

# Build a local Python wheel.
package-wheel +args: _project-temp
    maturin build --release --locked --out dist {{ args }}

# Build a local source distribution.
package-sdist output="dist": _project-temp
    maturin sdist --out "{{ output }}"

coverage-native output=".tox/coverage/native.lcov": test-deps _docker-ready
    mkdir -p "$(dirname "{{ output }}")"
    cargo llvm-cov clean --workspace
    cargo llvm-cov --workspace --all-features --bench '*' --no-report
    PATH="{{ tools_root }}/bin:$PATH" cargo llvm-cov nextest --workspace \
      --all-features --profile ci --lib --bins --tests --examples \
      -E 'not(test(e2e_live))' --no-report
    cargo llvm-cov report --no-default-ignore-filename-regex \
      --ignore-filename-regex '/(\.cargo/(registry|git)|\.rustup/toolchains|rustc/[0-9a-f]+)/' \
      --fail-uncovered-lines 0 --show-missing-lines --lcov --output-path "{{ output }}"

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
    export CARGO_TARGET_DIR="{{ justfile_directory() }}/.tox/coverage-target/frontend"
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
    rm -rf .tox/bench/scratch .tox/conformance.test .tox/docker/tmp .tox/frontend .tox/hawk/graph \
      .tox/site .tox/tmp \
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

ci: all

all: lint coverage docs
