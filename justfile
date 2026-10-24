# Copyright (c) 2026 Manuel Hilgert <hilman2@gmail.com>
#
# Local checks. `just fast` runs before every push; `just full` before a release.

set shell := ["bash", "-euo", "pipefail", "-c"]

# Test parametrizations must collect identically in every xdist worker.
export PYTHONHASHSEED := "0"

python := ".venv/bin/python"
weewx_source := "build/weewx-db-trial/weewx-source/src"
json_source := "build/weewx-db-trial/json-source/src"
library := if os() == "macos" { "target/release/libweewx_db.dylib" } else { "target/release/libweewx_db.so" }

default: fast

# Create the virtual environment, fetch the pinned references and build.
setup:
    python3 -m venv .venv
    {{python}} -m pip install -r requirements-test.txt
    python3 tools/fetch_references.py
    just build

# Build the CLI and the Python module in one Cargo run, as the release packager does.
build:
    cargo build --release --locked --features python
    install -m 644 {{library}} python/weewxdb/_weewxdb.abi3.so

lint:
    cargo clippy --all-targets --features python --locked -q -- -D warnings

test-rust:
    cargo nextest run --locked --tests --status-level fail --final-status-level fail

# Run the Python suite against one WeeWX source tree.
test-python source=weewx_source *args="tests":
    PYTHONPATH=python:{{source}} {{python}} -m pytest -q --tb=short -n auto {{args}}

# Lint, build and run every Rust and Python test against the pinned WeeWX.
fast: lint build test-rust (test-python weewx_source)

# Like fast, but runs only the Python tests affected by Python-only changes.
affected:
    #!/usr/bin/env bash
    set -euo pipefail
    changed=$( { git diff --name-only origin/main...HEAD; git diff --name-only HEAD; git ls-files --others --exclude-standard; } | sort -u)
    if grep -qE '^(src/|Cargo\.|build\.rs)' <<< "$changed"; then
        just fast
    else
        just build
        # testmon cannot see changes inside the Rust module, hence the guard above.
        PYTHONPATH=python:{{weewx_source}} {{python}} -m pytest -q --tb=short --testmon tests
    fi

# Everything, including the WeeWX 5.6 JSON generator tree.
full: fast (test-python json_source)
