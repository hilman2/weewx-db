# WeeWX database

Experimental Rust column store, SQL CLI and Python/weedb adapter for WeeWX.
The current package version is 0.2.0. This repository is not a production release.

## Commands

Run from the repository root. `just setup` creates `.venv`, fetches the pinned
references and builds once.

- `just fast`: clippy, release build with the python feature, Rust tests with
  nextest, Python tests with xdist against `weewx-source`. About 5 s without
  changes, 16 s after a Rust change, 25 s cold.
- `just affected`: like fast; after Python-only changes pytest runs with testmon.
- `just full`: fast plus the Python tests against `json-source` (WeeWX 5.6).
- `just build`: `cargo build --release --locked --features python` and a copy of
  the library to `python/weewxdb/_weewxdb.abi3.so`. `tests/conftest.py` stops the
  session when the CLI, library or module is missing or older than `src/`.
- Single tests: `just test-python build/weewx-db-trial/weewx-source/src "tests/test_x.py -k name"`
  or `cargo nextest run --locked --test storage -E 'test(name)'`.

`python3 tools/fetch_references.py` obtains the pinned public reference checkouts.
A plain `cargo build --release` without `--features python` produces a library
that `tests/test_extension.py` cannot load.

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

## Release builds

The GitHub Actions release matrix covers Linux x86_64, ARM64, ARMv7 and both
macOS architectures. It runs on manual dispatch or a deliberate `v*` tag, never
on ordinary branch pushes or pull requests. Tag builds prepare a draft release.
Do not create or push release tags unless the user explicitly requests it.

- `python3 tools/build_extension.py` builds a native extension archive.
- `python3 tools/build_extension.py --target TRIPLE --glibc 2.17` builds a portable Linux archive with cargo-zigbuild.
- `python3 tools/check_extension.py ARCHIVE.zip` checks a built archive in a fresh interpreter.
