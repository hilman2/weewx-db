"""Fetch the public, pinned reference sources used by tests and benchmarks."""

from __future__ import annotations

import argparse
import json
import pathlib
import subprocess


def checkout(repository: str, revision: str, destination: pathlib.Path) -> None:
    """Create a pinned checkout, or verify an existing unmodified checkout."""
    if destination.exists():
        head = subprocess.run(['git', '-C', str(destination), 'rev-parse', '--verify', 'HEAD'],
                              capture_output=True, text=True)
        if head.returncode == 0:
            dirty = subprocess.check_output(['git', '-C', str(destination), 'status',
                                             '--porcelain', '--untracked-files=no'], text=True)
            if head.stdout.strip() != revision or dirty:
                raise RuntimeError('Reference checkout differs from the pin: %s' % destination)
            print('Verified %s at %s' % (destination.name, revision), flush=True)
            return
        remote = subprocess.check_output(['git', '-C', str(destination), 'remote',
                                          'get-url', 'origin'], text=True).strip()
        if remote != repository:
            raise RuntimeError('Reference origin differs: %s' % destination)
    else:
        destination.mkdir(parents=True)
        subprocess.run(['git', 'init', '--quiet', str(destination)], check=True)
        subprocess.run(['git', '-C', str(destination), 'remote', 'add', 'origin', repository], check=True)
    subprocess.run(['git', '-C', str(destination), 'fetch', '--quiet', '--depth=1',
                    'origin', revision], check=True)
    subprocess.run(['git', '-C', str(destination), 'checkout', '--quiet', '--detach',
                    revision], check=True)
    print('Fetched %s at %s' % (destination.name, revision), flush=True)


def main() -> None:
    root = pathlib.Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=pathlib.Path, default=root / 'build/weewx-db-trial')
    options = parser.parse_args()
    references = json.loads((root / 'references.json').read_text())
    for name, reference in references.items():
        checkout(reference['repository'], reference['revision'], options.root / name)


if __name__ == '__main__':
    main()
