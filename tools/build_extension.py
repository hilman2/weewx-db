"""Build a complete, offline-installable WeeWX extension for native and cross-compiled targets."""

from __future__ import annotations

import argparse
import hashlib
import os
import re
import struct
import json
import platform
import shutil
import subprocess
import sysconfig
import tempfile
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


TARGETS = {
    'x86_64-unknown-linux-gnu': ('Linux', 'x86_64', 2, 62),
    'aarch64-unknown-linux-gnu': ('Linux', 'aarch64', 2, 183),
    'armv7-unknown-linux-gnueabihf': ('Linux', 'armv7l', 1, 40),
    'x86_64-apple-darwin': ('Darwin', 'x86_64', 2, 0x01000007),
    'aarch64-apple-darwin': ('Darwin', 'arm64', 2, 0x0100000c),
}


def check_binary(path: Path, target: str, glibc: str | None = None) -> None:
    system, _, elf_class, machine = TARGETS[target]
    header = path.read_bytes()[:32]
    if system == 'Linux':
        if len(header) < 20 or header[:4] != b'\x7fELF' or header[4] != elf_class or header[5] != 1 \
                or struct.unpack_from('<H', header, 18)[0] != machine:
            raise ValueError('Binary architecture does not match %s: %s' % (target, path))
        if glibc is not None:
            info = subprocess.check_output(['readelf', '-W', '--version-info', str(path)], text=True)
            versions = [tuple(map(int, version.split('.'))) for version in re.findall(r'Name: GLIBC_([0-9.]+)', info)]
            if not versions or max(versions) > tuple(map(int, glibc.split('.'))):
                raise ValueError('Binary requires newer glibc than %s: %s' % (glibc, path))
    elif len(header) < 8 or header[:4] != b'\xcf\xfa\xed\xfe' \
            or struct.unpack_from('<I', header, 4)[0] != machine:
        raise ValueError('Binary architecture does not match %s: %s' % (target, path))


def build(output: Path, compile_rust: bool = True, target: str | None = None,
          glibc: str | None = None) -> Path:
    if target is not None and target not in TARGETS:
        raise ValueError('Unsupported release target: %s' % target)
    if glibc is not None and (target is None or TARGETS[target][0] != 'Linux'):
        raise ValueError('--glibc requires an explicit Linux GNU target')
    system, machine = TARGETS[target][:2] if target else (platform.system(), platform.machine())
    if system == 'Linux' and machine in ['aarch64', 'arm64', 'armv8l'] and struct.calcsize('P') == 4:
        machine = 'armv7l'
    if system not in ['Linux', 'Darwin']:
        raise RuntimeError('Extension archives currently support Linux and macOS')
    if compile_rust:
        command = ['cargo', 'zigbuild' if glibc else 'build', '--release', '--locked', '--features', 'python']
        if target:
            command.extend(['--target', target + ('.' + glibc if glibc else '')])
        environment = dict(os.environ)
        if target:
            environment['PYO3_NO_PYTHON'] = '1'
        subprocess.run(command, cwd=ROOT, env=environment, check=True)
    metadata = json.loads(subprocess.check_output(['cargo', 'metadata', '--no-deps', '--format-version', '1'], cwd=ROOT))
    version = metadata['packages'][0]['version']
    release = Path(metadata['target_directory'])
    if target:
        release /= target
    release /= 'release'
    library = release / ('libweewx_db.dylib' if system == 'Darwin' else 'libweewx_db.so')
    executable = release / 'weewx-db'
    if target:
        check_binary(library, target, glibc)
        check_binary(executable, target, glibc)
    name = 'weewx-db-%s-%s' % (version, target or sysconfig.get_platform())
    output.mkdir(parents=True, exist_ok=True)
    destination = output.resolve() / (name + '.zip')
    with tempfile.TemporaryDirectory(prefix='weewxdb-extension-') as temporary:
        root = Path(temporary) / name
        package = root / 'bin/user/weewxdb'
        package.mkdir(parents=True)
        for source in (ROOT / 'python/weewxdb').glob('*.py'):
            shutil.copy2(source, package / source.name)
        shutil.copy2(library, package / '_weewxdb.abi3.so')
        shutil.copy2(executable, package / 'weewx-db')
        for name in ['LICENSE', 'NOTICE.md', 'SOURCE_SNAPSHOT.json', 'references.json', 'astronomy.md',
                     'vendor/LICENSE.ephem', 'vendor/LICENSE.xaggs']:
            destination_file = package / name
            destination_file.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(ROOT / name, destination_file)
        (package / 'build.json').write_text(json.dumps({
            'version': version, 'system': system, 'machine': machine, 'target': target,
            'build_host': platform.platform(), 'minimum_glibc': glibc,
            'python_abi': 'CPython 3.9+ abi3'}, indent=2) + '\n')
        files = sorted(str(path.relative_to(root)) for path in package.rglob('*') if path.is_file())
        installer = (ROOT / 'extension/install.py.in').read_text()
        installer = installer.replace('@FILES@', repr([('bin/user/weewxdb', files)]))
        installer = installer.replace('@PLATFORM@', repr((system, machine)))
        installer = installer.replace('@VERSION@', repr(version))
        (root / 'install.py').write_text(installer)
        shutil.copy2(ROOT / 'extension.md', root / 'README.md')
        shutil.copy2(ROOT / 'batch.md', root / 'batch.md')
        # Include the matching sources so an archive can be rebuilt on ARM or
        # another distribution without relying on a moving repository branch.
        source_root = root / 'source'
        names = ['Cargo.toml', 'Cargo.lock', 'build.rs', 'pyproject.toml', 'requirements-test.txt',
                 'LICENSE', 'NOTICE.md', 'SOURCE_SNAPSHOT.json', 'references.json',
                 'README.md', 'extension.md', 'batch.md', 'astronomy.md', 'architecture.md', 'reference.md']
        source_files = [ROOT / name for name in names]
        for directory, pattern in [('src', '*.rs'), ('python', '*.py'), ('tools', '*.py'),
                                   ('tests', '*.py'), ('tests', '*.rs'), ('extension', '*.in'), ('vendor', 'LICENSE*'), ('.github', '*.yml')]:
            source_files.extend((ROOT / directory).rglob(pattern))
        for source in source_files:
            source_destination = source_root / source.relative_to(ROOT)
            source_destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source, source_destination)
        with zipfile.ZipFile(destination, 'w', zipfile.ZIP_DEFLATED) as archive:
            for path in sorted(root.rglob('*')):
                if path.is_file():
                    archive.write(path, path.relative_to(root.parent))
    digest = hashlib.sha256(destination.read_bytes()).hexdigest()
    destination.with_suffix('.zip.sha256').write_text('%s  %s\n' % (digest, destination.name))
    return destination


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, default=ROOT / 'dist')
    parser.add_argument('--no-build', action='store_true', help='Package the existing local release build')
    parser.add_argument('--target', choices=TARGETS, help='Rust target triple for build and packaging')
    parser.add_argument('--glibc', help='Build with cargo-zigbuild and verify this glibc ceiling')
    args = parser.parse_args()
    print(build(args.output, not args.no_build, args.target, args.glibc))


if __name__ == '__main__':
    main()
