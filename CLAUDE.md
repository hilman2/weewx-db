# WeeWX database

Experimental Rust column store, SQL CLI and Python/weedb adapter for WeeWX.
The current package version is 0.2.0. This repository is not a production release.

## Commands

Run from the repository root after the development setup in README.md:

- `cargo test --locked --tests`
- `cargo clippy --all-targets --features python --locked -- -D warnings`
- `cargo build --release --locked`
- `PYTHONPATH=build/weewx-db-trial/weewx-source/src .venv/bin/python -m pytest -q tests`

The Python integration tests need the installed extension and release CLI.
`python3 tools/fetch_references.py` obtains the pinned public reference checkouts.

## Layout

- `src/`: Rust storage, SQL, weather calculations and solar/lunar astronomy.
- `python/weewxdb/`: Python DB-API, weedb, XType and almanac adapters.
- `tests/`: Rust and Python differential tests.
- `tools/`: Reference setup, benchmark and coefficient-import tools.
- `results/`: Measurement reports and reproduction instructions.
- `SOURCE_SNAPSHOT.json`: Hashes of the original prototype files in the first commit.

## Conventions

Keep the existing style. Do not run automatic formatters over unrelated code.
Python adapters follow WeeWX's conventions. Documentation is in English.
Preserve upstream notices and pinned numerical references when editing astronomy.
Keep generated databases, credentials, virtual environments and build products out of Git.
