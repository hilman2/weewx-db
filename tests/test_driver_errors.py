# Copyright (c) 2026 Manuel Hilgert <hilman2@gmail.com>
"""Failures reach WeeWX as weedb exceptions, which weewxd survives."""

import contextlib
import fcntl
import subprocess
import sys
import time

import pytest

import weedb
import weeutil.weeutil
import weewx
import weewx.manager
import weewx.xtypes

from weewxdb import driver

SCHEMA = [('dateTime', 'INTEGER NOT NULL UNIQUE PRIMARY KEY'), ('usUnits', 'INTEGER NOT NULL'),
          ('interval', 'INTEGER NOT NULL'), ('outTemp', 'REAL'), ('windSpeed', 'REAL'), ('windDir', 'REAL')]
RECORDS = [{'dateTime': 3600 * i, 'usUnits': weewx.METRIC, 'interval': 60, 'outTemp': float(i),
            'windSpeed': 2.0, 'windDir': 90.0 if i % 2 else 180.0} for i in range(1, 49)]


@pytest.fixture
def database(tmp_path):
    binding = {'driver': 'weewxdb.driver', 'database_name': str(tmp_path / 'errors.wwdb'), 'timeout': '0.2'}
    with weewx.manager.Manager.open_with_create(binding, schema=SCHEMA) as manager:
        manager.addRecord(RECORDS)
    return binding


@contextlib.contextmanager
def writer_lock(binding):
    # The writer lock is a flock on the sidecar; another open file
    # description conflicts with it even inside this process.
    with open(binding['database_name'] + '.lock', 'a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        yield


def test_lock_timeout_on_open_is_a_weedb_error(database):
    with writer_lock(database):
        started = time.monotonic()
        with pytest.raises(weedb.OperationalError, match='database is locked'):
            weedb.connect(database)
        # The configured timeout applies to opening, too.
        assert time.monotonic() - started < 2


def test_lock_timeout_in_an_xtype_is_a_weedb_error(database):
    with weewx.manager.Manager.open(database) as reader, weewx.manager.Manager.open(database) as other:
        span = weeutil.weeutil.TimeSpan(0, 48 * 3600)
        assert weewx.xtypes.get_aggregate('outTemp', span, 'max', reader)[0] == 48.0
        # A commit elsewhere makes the reader refresh, which needs the lock.
        other.addRecord(dict(RECORDS[-1], dateTime=49 * 3600))
        with writer_lock(database), pytest.raises(weedb.OperationalError, match='database is locked'):
            weewx.xtypes.get_series('outTemp', span, reader, 'avg', 7200)


def test_errors_carry_their_cause(database):
    path = database['database_name']
    with open(path, 'r+b') as file:
        # Bytes 8 to 16 of the header hold the offset of the index.
        offset = int.from_bytes(file.read(16)[8:], 'little')
        file.seek(offset)
        file.write(b'\xff')
    # Each process checks an index once, so a new process has to look.
    code = '''
import sys, weedb
try:
    weedb.connect({'driver': 'weewxdb.driver', 'database_name': sys.argv[1]})
except weedb.OperationalError as error:
    print(error)
'''
    result = subprocess.run([sys.executable, '-c', code, path], capture_output=True, text=True, check=True)
    assert 'index checksum mismatch' in result.stdout


def test_vector_average_through_weewx_sql(database, monkeypatch):
    # WeeWX's own SQL for vecdir needs RADIANS, SIN and COS.
    monkeypatch.setattr(weewx.xtypes, 'xtypes', [weewx.xtypes.ArchiveTable()])
    with weewx.manager.Manager.open(database) as manager:
        assert manager.connection.has_math
        direction = weewx.xtypes.get_aggregate('wind', weeutil.weeutil.TimeSpan(0, 48 * 3600), 'vecdir', manager)
        assert direction[0] == pytest.approx(135.0)


def test_weedb_classes_exist_on_every_supported_weewx():
    assert issubclass(driver.DatabaseLockedError, weedb.OperationalError)
