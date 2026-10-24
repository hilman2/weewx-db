# Copyright (c) 2026 Manuel Hilgert <hilman2@gmail.com>
"""Damaged files raise ordinary errors and fail the check; nothing panics."""

import os
import pathlib
import sqlite3
import subprocess
import sys
import zlib

import weewxdb

ROOT = pathlib.Path(__file__).resolve().parents[1]


def test_null_timestamp_with_a_valid_checksum(tmp_path):
    path = tmp_path / 'damaged.wwdb'
    weewxdb._weewxdb.create(str(path))
    connection = weewxdb.connect(path)
    connection.execute('CREATE TABLE archive(dateTime INTEGER PRIMARY KEY, value REAL)').close()
    connection.executemany('INSERT INTO archive VALUES (?, ?)', [(300, 1.0), (600, 2.0), (900, 3.0)])
    connection.native.checkpoint(False)
    connection.close()
    # Mark the first timestamp NULL and give the payload a matching CRC, as
    # a crafted file could. The dateTime descriptor comes first in the block:
    # chunk id (8 bytes), length (4), CRC (4), flags, summary.
    raw = sqlite3.connect(path)
    descriptors = bytearray(raw.execute('SELECT columns FROM blocks').fetchone()[0])
    chunk = int.from_bytes(descriptors[:8], 'little')
    payload = bytearray(raw.execute('SELECT payload FROM chunks WHERE id = ?', (chunk,)).fetchone()[0])
    payload[0] &= 0b11111110
    descriptors[12:16] = zlib.crc32(payload).to_bytes(4, 'little')
    raw.execute('UPDATE chunks SET payload = ? WHERE id = ?', (bytes(payload), chunk))
    raw.execute('UPDATE blocks SET columns = ?', (bytes(descriptors),))
    raw.commit()
    raw.close()
    code = '''
import sys, weewxdb
connection = weewxdb.connect(sys.argv[1])
try:
    connection.execute('SELECT value FROM archive WHERE dateTime = 600').fetchall()
except weewxdb.OperationalError as error:
    print('error:', error)
'''
    result = subprocess.run([sys.executable, '-c', code, str(path)], capture_output=True, text=True,
                            env=dict(os.environ, PYTHONPATH=os.pathsep.join(sys.path)))
    assert result.returncode == 0, result.stderr
    # The binary search over timestamps panics on the NULL; the process
    # survives and the caller sees an ordinary error.
    assert result.stdout.startswith('error: internal error'), result.stdout
    check = subprocess.run([str(ROOT / 'target/release/weewx-db'), 'check', str(path)], capture_output=True, text=True)
    assert check.returncode == 1 and 'NULL dateTime' in check.stderr, check.stderr
