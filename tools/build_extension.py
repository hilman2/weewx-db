"""Build a complete, offline-installable WeeWX extension for the local platform."""

import argparse
import json
import platform
import shutil
import subprocess
import sysconfig
import tempfile
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def build(output: Path, compile_rust: bool = True) -> Path:
    if platform.system() not in ['Linux', 'Darwin']:
        raise RuntimeError('Extension archives currently support Linux and macOS')
    if compile_rust:
        subprocess.run(['cargo', 'build', '--release', '--locked', '--features', 'python'], cwd=ROOT, check=True)
    metadata = json.loads(subprocess.check_output(['cargo', 'metadata', '--no-deps', '--format-version', '1'], cwd=ROOT))
    version = metadata['packages'][0]['version']
    release = Path(metadata['target_directory']) / 'release'
    library = release / ('libweewx_db.dylib' if platform.system() == 'Darwin' else 'libweewx_db.so')
    name = 'weewx-db-%s-%s' % (version, sysconfig.get_platform())
    output.mkdir(parents=True, exist_ok=True)
    destination = output.resolve() / (name + '.zip')
    with tempfile.TemporaryDirectory(prefix='weewxdb-extension-') as temporary:
        root = Path(temporary) / name
        package = root / 'bin/user/weewxdb'
        package.mkdir(parents=True)
        for source in (ROOT / 'python/weewxdb').glob('*.py'):
            shutil.copy2(source, package / source.name)
        shutil.copy2(library, package / '_weewxdb.abi3.so')
        shutil.copy2(release / 'weewx-db', package / 'weewx-db')
        for name in ['LICENSE', 'NOTICE.md', 'SOURCE_SNAPSHOT.json', 'references.json', 'astronomy.md',
                     'vendor/LICENSE.ephem', 'vendor/LICENSE.xaggs']:
            destination_file = package / name
            destination_file.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(ROOT / name, destination_file)
        (package / 'build.json').write_text(json.dumps({
            'version': version, 'platform': platform.platform(), 'machine': platform.machine(),
            'libc': platform.libc_ver(), 'python_abi': 'CPython 3.9+ abi3'}, indent=2) + '\n')
        files = sorted(str(path.relative_to(root)) for path in package.rglob('*') if path.is_file())
        installer = (ROOT / 'extension/install.py.in').read_text()
        installer = installer.replace('@FILES@', repr([('bin/user/weewxdb', files)]))
        installer = installer.replace('@PLATFORM@', repr((platform.system(), platform.machine())))
        installer = installer.replace('@VERSION@', repr(version))
        (root / 'install.py').write_text(installer)
        shutil.copy2(ROOT / 'extension.md', root / 'README.md')
        shutil.copy2(ROOT / 'batch.md', root / 'batch.md')
        # Include the matching sources so an archive can be rebuilt on ARM or
        # another distribution without relying on a moving repository branch.
        source_root = root / 'source'
        names = ['Cargo.toml', 'Cargo.lock', 'pyproject.toml', 'requirements-test.txt',
                 'LICENSE', 'NOTICE.md', 'SOURCE_SNAPSHOT.json', 'references.json',
                 'README.md', 'extension.md', 'batch.md', 'astronomy.md', 'architecture.md', 'reference.md']
        source_files = [ROOT / name for name in names]
        for directory, pattern in [('src', '*.rs'), ('python', '*.py'), ('tools', '*.py'),
                                   ('tests', '*.py'), ('tests', '*.rs'), ('extension', '*.in'), ('vendor', 'LICENSE*')]:
            source_files.extend((ROOT / directory).rglob(pattern))
        for source in source_files:
            source_destination = source_root / source.relative_to(ROOT)
            source_destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source, source_destination)
        with zipfile.ZipFile(destination, 'w', zipfile.ZIP_DEFLATED) as archive:
            for path in sorted(root.rglob('*')):
                if path.is_file():
                    archive.write(path, path.relative_to(root.parent))
    return destination


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, default=ROOT / 'dist')
    parser.add_argument('--no-build', action='store_true', help='Package the existing local release build')
    args = parser.parse_args()
    print(build(args.output, not args.no_build))


if __name__ == '__main__':
    main()
