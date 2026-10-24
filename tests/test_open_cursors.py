# Copyright (c) 2026 Manuel Hilgert <hilman2@gmail.com>
"""Writes on a connection that still streams a read, as WeeWX's backfill does."""

import time

import pytest

import weedb
import weewx.manager

import weewxdb

RECORDS = [(300 * i, float(i)) for i in range(1, 1001)]


@pytest.fixture
def connection(tmp_path):
    path = tmp_path / 'cursors.wwdb'
    weewxdb._weewxdb.create(str(path))
    connection = weewxdb.connect(path)
    connection.execute('CREATE TABLE archive(dateTime INTEGER PRIMARY KEY, value REAL)').close()
    connection.executemany('INSERT INTO archive VALUES (?, ?)', RECORDS)
    yield connection
    connection.close()


def stream(connection):
    # A plain ordered SELECT is not answered natively, so it stays open as a
    # streaming statement while fewer rows than the result are fetched.
    cursor = connection.execute('SELECT dateTime, value FROM archive ORDER BY dateTime')
    cursor.arraysize = 50
    return cursor


def test_writes_inside_a_transaction_while_reading(connection):
    connection.execute('BEGIN').close()
    seen = []
    for time_, value in stream(connection):
        seen.append((time_, value))
        connection.execute('UPDATE archive SET value = ? WHERE dateTime = ?', (-value, time_)).close()
        connection.execute('INSERT INTO archive VALUES (?, ?)', (10 ** 6 + time_, value)).close()
    connection.execute('COMMIT').close()
    # The stream keeps the state it started with.
    assert seen == RECORDS
    assert connection.execute('SELECT COUNT(*), SUM(value) FROM archive').fetchone() == (2000, 0.0)


def test_automatic_transactions_while_reading(connection):
    cursor = stream(connection)
    assert cursor.fetchmany(10) == RECORDS[:10]
    connection.execute('DELETE FROM archive WHERE dateTime = ?', (RECORDS[-1][0],)).close()
    assert cursor.fetchall() == RECORDS[10:]
    assert connection.execute('SELECT COUNT(*) FROM archive').fetchone() == (999,)


def test_rollback_aborts_open_reads(connection):
    connection.execute('BEGIN').close()
    cursor = stream(connection)
    assert cursor.fetchmany(10) == RECORDS[:10]
    connection.execute('DELETE FROM archive').close()
    connection.execute('ROLLBACK').close()
    with pytest.raises(weewxdb.OperationalError, match='aborted'):
        cursor.fetchmany(10)
    assert connection.execute('SELECT COUNT(*) FROM archive').fetchone() == (1000,)


def test_schema_changes_wait_for_open_reads(connection):
    cursor = stream(connection)
    cursor.fetchmany(10)
    with pytest.raises(weewxdb.OperationalError, match='schema'):
        connection.execute('ALTER TABLE archive ADD COLUMN extra REAL')
    cursor.close()
    connection.execute('ALTER TABLE archive ADD COLUMN extra REAL').close()


@pytest.fixture
def utc(monkeypatch):
    monkeypatch.setenv('TZ', 'UTC')
    time.tzset()
    yield
    monkeypatch.undo()
    time.tzset()


def test_backfill_writes_every_day(tmp_path, utc):
    schema = [('dateTime', 'INTEGER NOT NULL UNIQUE PRIMARY KEY'), ('usUnits', 'INTEGER NOT NULL'),
              ('interval', 'INTEGER NOT NULL'), ('outTemp', 'REAL')]
    start = 1700006400
    records = [{'dateTime': start + 300 * i, 'usUnits': weewx.METRIC, 'interval': 5,
                'outTemp': float(i % 50)} for i in range(1, 6 * 288 + 1)]
    days = {}
    for name, database in [('sqlite', {'driver': 'weedb.sqlite', 'database_name': str(tmp_path / 'a.sdb')}),
                           ('rust', {'driver': 'weewxdb.driver', 'database_name': str(tmp_path / 'a.wwdb')})]:
        with weewx.manager.Manager.open_with_create(database, schema=schema) as manager:
            manager.addRecord(records)
        with weewx.manager.DaySummaryManager.open_with_create(database, schema=schema) as manager:
            assert manager.backfill_day_summary() == (len(records), 6)
            days[name] = manager.getSql('SELECT COUNT(*), SUM(count), SUM(sum) FROM archive_day_outTemp')
    assert days['rust'] == days['sqlite'] == (6, len(records), sum(r['outTemp'] for r in records))
    weedb.drop({'driver': 'weewxdb.driver', 'database_name': str(tmp_path / 'a.wwdb')})
