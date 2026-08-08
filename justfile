set shell := ["bash", "-euo", "pipefail", "-c"]

# Run the default test suite.
default: test

# Check Rust formatting and lints.
lint:
    cargo fmt --all --check --
    cargo clippy --workspace --all-targets -- -D warnings
    .github/scripts/check-test-layout

# Run workspace tests, benchmark harnesses, and doctests.
test:
    cargo nextest run --workspace --lib --bins --tests --examples
    cargo test --workspace --bench '*'
    cargo test --workspace --doc

# Run end-to-end tests against external service boundaries.
e2e:
    cargo nextest run -p peryx --features e2e

# Run distributed availability tests.
availability:
    cargo nextest run -p peryx --features availability-e2e

# Build and test the browser application.
frontend:
    npm --prefix tests/frontend ci
    cargo leptos build
    npm --prefix tests/frontend test

# Record workspace test coverage.
coverage-native output="coverage-native.lcov":
    .github/scripts/coverage-native "{{output}}"

# Record end-to-end test coverage.
coverage-e2e output="coverage-e2e.lcov":
    .github/scripts/coverage-e2e "{{output}}"

# Record distributed availability coverage.
coverage-availability output="coverage-availability.lcov":
    .github/scripts/coverage-availability "{{output}}"

# Record native and Wasm browser coverage.
coverage-frontend native="coverage-frontend-native.lcov" wasm="coverage-frontend-wasm.lcov":
    .github/scripts/coverage-frontend "{{native}}" "{{wasm}}"

# Merge LCOV reports and enforce complete coverage.
coverage-merge output +inputs:
    .github/scripts/coverage-merge "{{output}}" {{inputs}}

# Record and merge all Linux coverage reports.
coverage output=".tox/coverage":
    .github/scripts/coverage-linux "{{output}}"

# Run repository hooks against every file.
pre-commit:
    pre-commit run --all-files

# Run every validation target.
all: lint coverage pre-commit
