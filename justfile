set shell := ["bash", "-euo", "pipefail", "-c"]

project_tmp := justfile_directory() + "/.tox/tmp"
hawk_root := justfile_directory() + "/.tox/hawk"
coverage_root := justfile_directory() + "/.tox/coverage"
frontend_root := justfile_directory() + "/.tox/frontend"
export TMPDIR := project_tmp
export TMP := project_tmp
export TEMP := project_tmp
export PERYX_TEST_TMPDIR := project_tmp
export PLAYWRIGHT_BROWSERS_PATH := frontend_root + "/browsers"

# Run the default Rust test suite.
default: test

# Check Rust formatting.
format-check: _project-temp
    cargo fmt --all --check --

# Check ecosystem dependency boundaries.
ecosystem-boundaries: _project-temp
    .github/scripts/check-ecosystem-boundaries

# Check the Rust test layout.
test-layout: _project-temp
    .github/scripts/check-test-layout

# Reject nondeterministic timing in test code.
test-timing: _project-temp
    .github/scripts/check-test-timing

# Reject test processes and servers without cleanup ownership.
test-processes: _project-temp
    .github/scripts/check-test-processes

# Check all Rust targets with Clippy.
clippy: _project-temp
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# Reject unused public Rust items.
dead-public: _project-temp
    cargo +1.97.1 hawk check --manifest-path Cargo.toml --target-dir "{{hawk_root}}/target" --graph-dir "{{hawk_root}}/graph" --only dead-public --deny hawk::dead_public --output-format json

# Check Rust source without running tests.
lint-source: _project-temp format-check clippy dead-public

# Check rustdoc and Markdown.
lint-docs: _project-temp
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps
    prek run mdformat --all-files
    prek run codespell --all-files

# Check workflows, shell scripts, Compose, and repository hooks.
lint-automation: _project-temp compose-check
    .github/scripts/lint-automation
    SKIP=cargo-fmt,cargo-clippy,render-diagrams,mdformat,codespell,test-layout,test-timing prek run --all-files

# Check package and nested-workspace manifest policy.
manifest-policy: _project-temp
    scripts/ci/check-manifest-policy

# Check dependency policy and unused declarations.
lint-deps: _project-temp manifest-policy
    cargo deny check
    .github/scripts/check-cargo-shear

# Check crate features, snapshots, packages, and public APIs.
lint-contracts base="origin/main": _project-temp ecosystem-boundaries test-layout test-timing test-processes
    cargo hack --workspace --each-feature check --all-targets
    .github/scripts/check-snapshots "{{base}}"
    .github/scripts/package-crates
    .github/scripts/semver-check "{{base}}"
    just release-plan

# Run all lint lanes.
lint base="origin/main": _project-temp
    just lint-source
    just lint-docs
    just lint-automation
    just lint-deps
    just lint-contracts "{{base}}"

# Prepare the project-owned temporary directory.
_project-temp:
    mkdir -p "{{project_tmp}}"

# Run workspace tests and benchmark harnesses.
test: _project-temp
    cargo nextest run --workspace --exclude peryx-pypi-system-tests --exclude peryx-oci-system-tests --lib --bins --tests --examples
    just benchmark

# Run workspace benchmark harnesses with a process deadline.
benchmark timeout_seconds="1200": _project-temp
    @seconds="{{timeout_seconds}}"; \
      if [[ ! $seconds =~ ^[1-9][0-9]*$ ]]; then \
        printf 'benchmark deadline must be a positive integer\n' >&2; \
        exit 2; \
      fi; \
      status=0; \
      perl -e 'alarm shift; exec @ARGV or die "exec: $!\n"' "$seconds" \
        cargo test --workspace --all-features --bench '*' --no-fail-fast || status=$?; \
      if ((status == 142)); then \
        printf 'command timed out after %s seconds: cargo test --workspace --all-features --bench *\n' "$seconds" >&2; \
        exit 124; \
      fi; \
      exit "$status"

# Compile the workspace and run platform boundary tests.
platform-contract: _project-temp
    scripts/ci/platform-contract

# Check, test, and cover one crate.
crate-contract package output=".tox/crate-contracts": _project-temp
    .github/scripts/crate-contracts "{{output}}" "{{package}}"

# Check, test, and cover a crate group.
crate-contracts output +packages: _project-temp
    .github/scripts/crate-contracts "{{output}}" {{packages}}

# List workspace packages in an explicit scope.
workspace-package-list scope="all": _project-temp
    scripts/ci/workspace-package-list "{{scope}}"

# Split workspace crates into weighted contract or system shards.
workspace-packages shards="8" scope="contracts" timings="": _project-temp
    scripts/ci/workspace-packages "{{shards}}" "{{scope}}" "{{timings}}"

# Check GitHub job results against the workflow policy.
ci-gate policy: _project-temp
    scripts/ci/check-job-results "{{policy}}" "$CI_RESULTS" "$CI_CONTEXT"

# Capture local or CI runner diagnostics under .tox.
ci-diagnostics output=".tox/diagnostics/local": _project-temp
    bash scripts/ci/diagnostics "{{output}}"

# Verify Windows sccache response-file handling.
sccache-smoke:
    pwsh -NoProfile -NonInteractive -File scripts/ci/sccache-response-file.ps1

# Run hermetic client boundary tests.
e2e: _project-temp
    cargo nextest run -p peryx-pypi-system-tests --features e2e --test e2e -E 'not(test(e2e_live))'

# Run live client boundary tests.
e2e-live: _project-temp
    cargo nextest run -p peryx-pypi-system-tests --features e2e-live --test e2e -E 'test(e2e_live)'

# Run PyPI system tests that do not require external services.
pypi-system: _project-temp
    cargo nextest run -p peryx-pypi-system-tests --tests -E 'not(binary(e2e)) & not(binary(availability)) & not(binary(s3_upload))'

# Run OCI system tests that do not require a cluster.
oci-system: _project-temp
    cargo nextest run -p peryx-oci-system-tests --tests -E 'not(binary(availability))'

# Run the PyPI S3 boundary tests.
s3: _project-temp
    cargo nextest run -p peryx-pypi-system-tests --test s3_upload

# Run storage tests backed by S3 containers.
storage-s3: _project-temp
    cargo nextest run -p peryx-storage --features container-tests --test integration

# Run distributed availability tests.
availability: _project-temp
    cargo nextest run -p peryx --features availability-e2e --test availability --test cluster --test observability
    cargo nextest run -p peryx-pypi-system-tests --test availability
    cargo nextest run -p peryx-oci-system-tests --test availability

# Run an availability simulation selection.
simulation filter="all()": _project-temp
    cargo nextest run -p peryx --features sim-campaign --test sim_campaign -E '{{filter}}'

# Mutate Rust lines changed since a base revision.
mutation base jobs="2" in_place="false" part="1/1": _project-temp
    .github/scripts/mutation-diff "{{base}}" "{{jobs}}" "{{in_place}}" "{{part}}"

# Mutate the workspace, optionally selecting one cargo-mutants shard.
mutation-full jobs="2" shard="": _project-temp
    .github/scripts/mutation-full "{{jobs}}" "{{shard}}"

# Check direct dependency lower bounds on nightly Cargo.
direct-minimum: _project-temp
    .github/scripts/direct-minimum

# Interpret the pure core crates with Miri.
miri +packages="peryx-core peryx-pql peryx-policy": _project-temp
    .github/scripts/miri {{packages}}

# Run AddressSanitizer against the workspace.
sanitizer-address: _project-temp
    .github/scripts/sanitizer address

# Run ThreadSanitizer against the workspace.
sanitizer-thread: _project-temp
    .github/scripts/sanitizer thread

# Run one existing cargo-fuzz target.
fuzz package target seconds="60": _project-temp
    .github/scripts/fuzz "{{package}}" "{{target}}" "{{seconds}}"

# Run existing cargo-fuzz targets for one package.
fuzz-targets package seconds +targets: _project-temp
    for target in {{targets}}; do .github/scripts/fuzz "{{package}}" "$target" "{{seconds}}"; done

# Run owner-declared fuzz targets for one package.
fuzz-package package seconds="60" output=".tox/fuzz/{{package}}.lcov": _project-temp
    scripts/ci/fuzz-package "{{package}}" "{{seconds}}" "{{output}}"

# Install tools declared by one package.
package-tools package: _project-temp
    scripts/ci/package-tools "{{package}}"

# Install browser-test dependencies for the shared and owner suites.
frontend-deps: _project-temp
    npm --prefix crates/peryx-web/tests/frontend ci
    npm --prefix crates/peryx-ecosystem-pypi/tests/frontend ci
    npm --prefix crates/peryx-ecosystem-oci/tests/frontend ci

# Install Chromium and optional host dependencies for browser tests.
frontend-browser-deps *args: _project-temp
    npm --prefix crates/peryx-web/tests/frontend exec -- playwright install {{args}} chromium

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

# Build and test the browser application.
frontend: frontend-deps _project-temp
    just frontend-browser-deps
    cargo leptos build
    just frontend-test

# Stage the shared site shell and owner documentation.
site-stage: _project-temp
    site/scripts/stage.sh

# Check committed Mermaid partials against their source hashes.
diagrams: _project-temp
    node site/scripts/render_diagrams.mjs --check

# Regenerate committed Mermaid partials.
render-diagrams: _project-temp
    npm --prefix site ci
    npm --prefix site run render

# Build and validate the assembled documentation site.
docs: _project-temp diagrams site-stage
    mkdir -p .tox/site/static
    cargo run --quiet --package peryx --bin peryx -- openapi > .tox/site/static/openapi.json
    zola --root .tox/site check
    zola --root .tox/site build
    python3 .tox/site/scripts/inline_diagrams.py .tox/site/public

# Check links in the assembled site.
site-links: docs
    node .tox/site/scripts/check_external_links.mjs .tox/site

# Build the assembled site.
site: docs

# Validate the cargo-dist release plan.
release-plan: _project-temp
    cargo dist plan --output-format=json > /dev/null

# Run one package's external conformance suite.
conformance package suite binary="": _project-temp
    cargo build --bin peryx
    binary="{{binary}}"; \
      scripts/ci/conformance "{{package}}" "{{suite}}" \
        "${binary:-${CARGO_TARGET_DIR:-target}/debug/peryx}"

# Run one CodSpeed benchmark in the CI container.
codspeed package jobs="4": _project-temp
    .github/codspeed/run.sh "{{package}}" "{{jobs}}"

# Select CodSpeed benchmark legs from named owner changes.
codspeed-matrix event runner shared +changes: _project-temp
    @scripts/ci/codspeed-matrix "{{event}}" "{{runner}}" "{{shared}}" {{changes}}

# Hash a CodSpeed runtime revision.
codspeed-runtime-id revision: _project-temp
    scripts/ci/codspeed-runtime-id "{{revision}}"

# Name a CodSpeed image from its definition.
codspeed-image-tag image: _project-temp
    scripts/ci/codspeed-image-tag "{{image}}"

# Hash benchmark source state.
codspeed-source-key: _project-temp
    scripts/ci/codspeed-source-key

# Preserve compatible CodSpeed cache timestamps.
codspeed-preserve-cache current restored: _project-temp
    scripts/ci/codspeed-preserve-cache "{{current}}" "{{restored}}"

# Record CodSpeed source state.
codspeed-record-sources: _project-temp
    scripts/ci/codspeed-record-sources

# Build a local Python wheel.
package-wheel +args: _project-temp
    scripts/ci/package-python wheel dist {{args}}

# Build a local source distribution.
package-sdist output="dist": _project-temp
    scripts/ci/package-python sdist "{{output}}"

# Record workspace test coverage.
coverage-native output=".tox/coverage/native.lcov": _project-temp
    .github/scripts/coverage-native "{{output}}"

# Record hermetic client coverage.
coverage-e2e output=".tox/coverage/e2e.lcov": _project-temp
    .github/scripts/coverage-e2e "{{output}}"

# Record live client coverage.
coverage-e2e-live output=".tox/coverage/e2e-live.lcov": _project-temp
    .github/scripts/coverage-e2e-live "{{output}}"

# Record ecosystem and client system coverage.
coverage-system-clients output=".tox/coverage/system-clients.lcov": _project-temp
    .github/scripts/coverage-system-clients "{{output}}"

# Record distributed and simulation system coverage.
coverage-system-distributed output=".tox/coverage/system-distributed.lcov": _project-temp
    .github/scripts/coverage-system-distributed "{{output}}"

# Record Docker-backed storage coverage.
coverage-system-storage output=".tox/coverage/system-storage.lcov": _project-temp
    .github/scripts/coverage-system-storage "{{output}}"

# Record distributed availability coverage.
coverage-availability output=".tox/coverage/availability.lcov": _project-temp
    .github/scripts/coverage-availability "{{output}}"

# Record availability simulation coverage.
coverage-simulation output=".tox/coverage/simulation.lcov": _project-temp
    .github/scripts/coverage-simulation "{{output}}"

# Record native and Wasm browser coverage.
coverage-frontend native=".tox/coverage/frontend-native.lcov" wasm=".tox/coverage/frontend-wasm.lcov" merged=".tox/coverage/frontend.lcov": _project-temp
    .github/scripts/coverage-frontend "{{native}}" "{{wasm}}" "{{merged}}"

# Merge LCOV reports and enforce complete coverage.
coverage-merge output +inputs: _project-temp
    .github/scripts/coverage-merge "{{output}}" {{inputs}}

# Record and merge all Linux coverage reports.
coverage output=".tox/coverage": _project-temp
    .github/scripts/coverage-linux "{{output}}"

# Remove local Rust coverage build artifacts and locks.
coverage-clean:
    bash scripts/ci/cleanup-workspace-artifacts coverage

# Remove transient project-owned artifacts.
clean:
    bash scripts/ci/cleanup-workspace-artifacts normal

# Remove project-owned artifacts, including reusable build state.
clean-all:
    bash scripts/ci/cleanup-workspace-artifacts all

# Run repository hooks against all files.
pre-commit: _project-temp
    prek run --all-files

# Run hooks whose CI equivalents do not need Cargo or browsers.
pre-commit-ci: _project-temp
    SKIP=cargo-fmt,cargo-clippy,render-diagrams prek run --all-files

# Prepare writable bind-mounted build caches.
_linux-dirs:
    mkdir -p .tox/docker/cache .tox/docker/cargo .tox/docker/data .tox/docker/home \
      .tox/docker/target .tox/docker/tmp "{{coverage_root}}" "{{frontend_root}}" "{{project_tmp}}"

# Validate the Linux test service definitions.
compose-check: _project-temp
    docker compose --profile test --profile system --profile analysis --profile 16g --profile system-16g --profile codspeed config --quiet

# Run a Just recipe in the lightweight Linux container.
linux +args: _linux-dirs
    PERYX_UID="$(id -u)" PERYX_GID="$(id -g)" docker compose --profile test run --rm test {{args}}

# Run a Just recipe with Docker-backed Linux services.
linux-system +args: _linux-dirs
    PERYX_UID="$(id -u)" PERYX_GID="$(id -g)" bash scripts/ci/compose-run system system {{args}}

# Run a dynamic-analysis recipe in Linux.
linux-analysis +args: _linux-dirs
    PERYX_UID="$(id -u)" PERYX_GID="$(id -g)" docker compose --profile analysis run --rm test {{args}}

# Run a Just recipe with a 16 GiB Linux memory limit.
linux-16g +args: _linux-dirs
    PERYX_UID="$(id -u)" PERYX_GID="$(id -g)" docker compose --profile 16g run --rm test-16g {{args}}

# Run a Just recipe with Docker-backed services within a 16 GiB limit.
linux-system-16g +args: _linux-dirs
    PERYX_UID="$(id -u)" PERYX_GID="$(id -g)" PERYX_DOCKER_MEMORY=4g \
      bash scripts/ci/compose-run system-16g system-16g {{args}}

# Remove Docker-backed Linux services.
linux-system-clean:
    bash scripts/ci/compose-run clean

# Rebuild the Linux test image from current upstream images and print tool versions.
linux-image:
    docker compose --profile test build --pull test
    PERYX_UID="$(id -u)" PERYX_GID="$(id -g)" docker compose --profile test run --rm test versions

# Run the full validation suite in the Linux test image.
ci: _linux-dirs
    just linux all

# Run the default system test lanes.
system-test: pypi-system oci-system e2e e2e-live s3 storage-s3 availability simulation frontend

# Run every local validation gate without repeating tests covered by coverage.
all: lint coverage docs pre-commit-ci
