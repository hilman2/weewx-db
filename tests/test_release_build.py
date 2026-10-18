"""Reject mislabeled binaries and keep cross-build metadata independent of the host."""

import importlib.util
import json
import struct
import zipfile
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]


@pytest.fixture
def builder():
    spec = importlib.util.spec_from_file_location('release_builder', ROOT / 'tools/build_extension.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def elf(machine, width):
    header = bytearray(32)
    header[:4] = b'\x7fELF'
    header[4] = width
    header[5] = 1
    struct.pack_into('<H', header, 18, machine)
    return header


def test_foreign_archive_uses_target_paths_and_metadata(builder, monkeypatch, tmp_path):
    target = 'armv7-unknown-linux-gnueabihf'
    release = tmp_path / 'target' / target / 'release'
    release.mkdir(parents=True)
    for name in ['libweewx_db.so', 'weewx-db']:
        (release / name).write_bytes(elf(40, 1))
    original = builder.subprocess.check_output
    def metadata(command, *args, **kwargs):
        if command[:2] == ['cargo', 'metadata']:
            return json.dumps({'packages': [{'version': '0.2.0'}],
                               'target_directory': str(tmp_path / 'target')}).encode()
        return original(command, *args, **kwargs)
    monkeypatch.setattr(builder.subprocess, 'check_output', metadata)
    archive = builder.build(tmp_path / 'dist', False, target)
    assert target in archive.name
    with zipfile.ZipFile(archive) as bundle:
        metadata = json.loads(next(bundle.read(name) for name in bundle.namelist() if name.endswith('/build.json')))
        assert metadata['machine'] == 'armv7l'
        assert metadata['target'] == target
        installer = next(bundle.read(name).decode() for name in bundle.namelist() if name.endswith('/install.py'))
        assert "PLATFORM = ('Linux', 'armv7l')" in installer
    assert archive.with_suffix('.zip.sha256').read_text().endswith(archive.name + '\n')


@pytest.mark.parametrize('machine,width', [(62, 2), (183, 2), (40, 2)])
def test_wrong_linux_architecture_rejected(builder, tmp_path, machine, width):
    binary = tmp_path / 'binary'
    binary.write_bytes(elf(machine, width))
    with pytest.raises(ValueError, match='architecture does not match'):
        builder.check_binary(binary, 'armv7-unknown-linux-gnueabihf')


def test_glibc_ceiling_checks_the_actual_binary(builder, monkeypatch, tmp_path):
    binary = tmp_path / 'binary'
    binary.write_bytes(elf(62, 2))
    monkeypatch.setattr(builder.subprocess, 'check_output', lambda *a, **k: 'Name: GLIBC_2.2.5\nName: GLIBC_2.34\n')
    with pytest.raises(ValueError, match='requires newer glibc'):
        builder.check_binary(binary, 'x86_64-unknown-linux-gnu', '2.17')
    monkeypatch.setattr(builder.subprocess, 'check_output', lambda *a, **k: 'Name: GLIBC_2.2.5\nName: GLIBC_2.17\n')
    builder.check_binary(binary, 'x86_64-unknown-linux-gnu', '2.17')


def test_macos_architecture_rejected_before_packaging(builder, tmp_path):
    binary = tmp_path / 'binary'
    binary.write_bytes(struct.pack('<II', 0xfeedfacf, 0x01000007))
    builder.check_binary(binary, 'x86_64-apple-darwin')
    with pytest.raises(ValueError, match='architecture does not match'):
        builder.check_binary(binary, 'aarch64-apple-darwin')
