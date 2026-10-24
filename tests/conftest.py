# Copyright (c) 2026 Manuel Hilgert <hilman2@gmail.com>
"""Refuse to run the suite against binaries that were not built from this tree."""

import importlib.util
import pathlib
import sys

import pytest

ROOT = pathlib.Path(__file__).resolve().parents[1]


def _newest_source():
    sources = [ROOT / 'Cargo.toml', ROOT / 'Cargo.lock', ROOT / 'build.rs']
    sources.extend((ROOT / 'src').rglob('*.rs'))
    return max(path.stat().st_mtime for path in sources)


def _problems():
    library = ROOT / ('target/release/libweewx_db.dylib' if sys.platform == 'darwin'
                      else 'target/release/libweewx_db.so')
    spec = importlib.util.find_spec('weewxdb._weewxdb')
    module = pathlib.Path(spec.origin) if spec and spec.origin else None
    binaries = {'CLI': ROOT / 'target/release/weewx-db', 'packaged library': library,
                'imported module': module}
    newest = _newest_source()
    for name, path in binaries.items():
        if path is None or not path.is_file():
            yield '%s is missing' % name
        elif path.stat().st_mtime < newest:
            yield '%s %s is older than the Rust sources' % (name, path)
    # tests/test_extension.py packs the release library as the extension module.
    # `cargo build` without the python feature produces one that cannot load.
    if library.is_file() and b'PyInit__weewxdb' not in library.read_bytes():
        yield '%s lacks the Python module entry point' % library


def pytest_sessionstart(session):
    problems = list(_problems())
    if problems:
        pytest.exit('Run `just build` first: ' + '; '.join(problems), returncode=4)
