# Copyright (c) 2026 Manuel Hilgert <hilman2@gmail.com>
"""Failures reach WeeWX as weedb exceptions, which weewxd survives."""

import contextlib
import sqlite3
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
    # Another process that has begun writing holds SQLite's write lock. In
    # this process, a second SQLite library would not see that lock.
    code = 'import sqlite3, sys; c = sqlite3.connect(sys.argv[1], isolation_level=None); ' \
           'c.execute("BEGIN IMMEDIATE"); print("locked", flush=True); sys.stdin.read()'
    holder = subprocess.Popen([sys.executable, '-c', code, binding['database_name']],
                              stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
    try:
        assert holder.stdout.readline().strip() == 'locked'
        yield
    finally:
        holder.stdin.close()
        holder.wait()


def test_readers_are_not_blocked_by_a_writer(database):
    with writer_lock(database):
        started = time.monotonic()
        with weewx.manager.Manager.open(database) as reader:
            span = weeutil.weeutil.TimeSpan(0, 48 * 3600)
            assert weewx.xtypes.get_aggregate('outTemp', span, 'max', reader)[0] == 48.0
            assert weewx.xtypes.get_series('outTemp', span, reader, 'avg', 7200)[2][0][-1] == 47.5
        assert time.monotonic() - started < 1


def test_lock_timeout_of_a_writer_is_a_weedb_error(database):
    with weewx.manager.Manager.open(database) as manager:
        with writer_lock(database):
            started = time.monotonic()
            # WeeWX's addRecord retries this exception with a backoff of seconds.
            with pytest.raises(driver.DatabaseLockedError, match='database is locked'):
                with weedb.Transaction(manager.connection) as cursor:
                    cursor.execute('DELETE FROM archive')
            assert time.monotonic() - started < 2
        manager.addRecord(dict(RECORDS[-1], dateTime=49 * 3600))


def test_errors_carry_their_cause(database):
    path = database['database_name']
    connection = sqlite3.connect(path)
    connection.execute("UPDATE tables SET columns = x'00'")
    connection.commit()
    connection.close()
    # Each process shares an index once read, so a new process has to look.
    code = '''
import sys, weedb
try:
    weedb.connect({'driver': 'weewxdb.driver', 'database_name': sys.argv[1]})
except weedb.OperationalError as error:
    print(error)
'''
    result = subprocess.run([sys.executable, '-c', code, path], capture_output=True, text=True, check=True)
    assert 'columns of table archive' in result.stdout


def test_vector_average_through_weewx_sql(database, monkeypatch):
    # WeeWX's own SQL for vecdir needs RADIANS, SIN and COS.
    monkeypatch.setattr(weewx.xtypes, 'xtypes', [weewx.xtypes.ArchiveTable()])
    with weewx.manager.Manager.open(database) as manager:
        assert manager.connection.has_math
        direction = weewx.xtypes.get_aggregate('wind', weeutil.weeutil.TimeSpan(0, 48 * 3600), 'vecdir', manager)
        assert direction[0] == pytest.approx(135.0)


def test_weedb_classes_exist_on_every_supported_weewx():
    assert issubclass(driver.DatabaseLockedError, weedb.OperationalError)
