"""Exercise the Python DB-API and WeeWX manager on the writable Rust engine."""

import json
import pathlib
import subprocess
import os
import sys
import time

import pytest

weewxdb = pytest.importorskip('weewxdb')

import weedb
import weewx
import weewx.manager
import weewx.schemas.wview_small
import weewx.xtypes
import weeutil.weeutil
import weewx.accum


def test_dbapi_streams_and_preserves_read_snapshot(tmp_path):
    path = tmp_path / 'live.wwdb'
    weewxdb._weewxdb.create(str(path))
    connection = weewxdb.connect(path, cache_mib=0)
    connection.execute('CREATE TABLE archive(dateTime INTEGER PRIMARY KEY, value REAL)').close()
    connection.executemany('INSERT INTO archive VALUES (?,?)',
                           [(index, float(index)) for index in range(5000)]).close()
    cursor = connection.execute('SELECT * FROM archive ORDER BY dateTime')
    assert cursor.fetchmany(2) == [(0, 0.0), (1, 1.0)]
    assert connection.execute('SELECT SUM(value) FROM archive').fetchone() == (12497500.0,)
    writer = weewxdb.connect(path, cache_mib=0)
    writer.execute('INSERT INTO archive VALUES (6000,1.5)').close()
    assert len(cursor.fetchall()) == 4998
    cursor.close()
    assert connection.execute('SELECT COUNT(*) FROM archive').fetchone() == (5001,)
    writer.close()
    connection.close()


def test_dbapi_transaction_failure_and_schema_extension(tmp_path):
    path = tmp_path / 'live.wwdb'
    weewxdb._weewxdb.create(str(path))
    connection = weewxdb.connect(path)
    connection.execute('CREATE TABLE archive(dateTime INTEGER PRIMARY KEY, value REAL)').close()
    with pytest.raises(weewxdb.IntegrityError):
        connection.executemany('INSERT INTO archive VALUES (?,?)', [(300, 1.0), (300, 2.0)])
    assert connection.execute('SELECT COUNT(*) FROM archive').fetchone() == (0,)
    with connection:
        connection.execute('INSERT INTO archive VALUES (300,2.0)').close()
        connection.execute('ALTER TABLE archive ADD COLUMN sensor REAL DEFAULT 0.0').close()
    assert connection.execute('SELECT sensor FROM archive').fetchone() == (0.0,)
    connection.native.checkpoint()
    connection.close()
    with weewxdb.connect(path) as restored:
        assert restored.execute('SELECT value,sensor FROM archive').fetchone() == (2.0, 0.0)


def test_native_weedb_manager_matches_sqlite(tmp_path):
    sqlite = {'driver': 'weedb.sqlite', 'database_name': str(tmp_path / 'source.sdb')}
    native = {'driver': 'weewxdb.driver', 'database_name': str(tmp_path / 'live.wwdb'),
              'cache_mib': 1}
    start = 1704067200
    records = [{'dateTime': start + index * 300, 'usUnits': weewx.METRICWX, 'interval': 5,
                'outTemp': None if index % 13 == 0 else float(index % 21 - 10),
                'outHumidity': 75.0, 'rain': 0.5 if index % 5 == 0 else 0.0,
                'windSpeed': 2.0, 'windDir': 270.0, 'windGust': 4.0, 'windGustDir': 180.0}
               for index in range(1, 401)]
    managers = [weewx.manager.DaySummaryManager.open_with_create(
        binding, schema=weewx.schemas.wview_small.schema) for binding in [sqlite, native]]
    try:
        for manager in managers:
            manager.addRecord(records, log_success=False, log_failure=False)
        span = weeutil.weeutil.TimeSpan(start, records[-1]['dateTime'])
        for observation in ['outTemp', 'rain', 'windSpeed', 'windvec']:
            for aggregate in ['avg', 'min', 'max'] if observation != 'windvec' else ['avg']:
                expected = weewx.xtypes.get_aggregate(observation, span, aggregate, managers[0])
                actual = weewx.xtypes.get_aggregate(observation, span, aggregate, managers[1])
                assert actual[1:] == expected[1:]
                assert actual[0] == pytest.approx(expected[0], rel=1e-12, abs=1e-12)
        for observation in ['outTemp', 'rain']:
            for aggregate in ['avg', 'sum', 'count', 'max', 'cumulative']:
                expected = weewx.xtypes.get_series(
                    observation, span, managers[0], aggregate_type=aggregate,
                    aggregate_interval=3600)
                actual = weewx.xtypes.get_series(
                    observation, span, managers[1], aggregate_type=aggregate,
                    aggregate_interval=3600)
                assert actual == expected
        assert list(managers[0].genBatchRecords()) == list(managers[1].genBatchRecords())
    finally:
        for manager in managers:
            manager.close()
    with weewx.manager.DaySummaryManager.open(native) as manager:
        assert len(list(manager.genBatchRecords())) == len(records)
    weedb.drop(native)


def test_cli_pipeline_and_csv(tmp_path):
    binary = pathlib.Path(__file__).resolve().parents[1] / 'target/release/weewx-db'
    if not binary.is_file():
        pytest.skip('Build the release CLI before the integration tests')
    path = tmp_path / 'live.wwdb'
    subprocess.run([str(binary), 'init', str(path)], check=True, capture_output=True)
    append = subprocess.run([str(binary), 'append', str(path), '--auto-add'],
                            input='{"dateTime":300,"outTemp":2.0,"pm25":8.0}\n'
                                  '{"dateTime":600,"outTemp":0.0,"pm25":null}\n',
                            text=True, capture_output=True, check=True)
    assert json.loads(append.stdout)['inserted'] == 2
    result = subprocess.run([str(binary), 'sql', str(path),
                             'SELECT dateTime,outTemp,pm25 FROM archive ORDER BY dateTime',
                             '--format', 'csv'], text=True, capture_output=True, check=True)
    assert result.stdout.splitlines() == ['dateTime,outTemp,pm25',
                                          '300,2.0,8.0', '600,0.0,']
    result = subprocess.run([str(binary), 'sql', str(path), '--',
                             "-- identifier and quoted semicolon\nSELECT interval, ';' AS marker FROM archive LIMIT 1; SELECT COUNT(*) FROM archive;"],
                            text=True, capture_output=True, check=True)
    lines = result.stdout.splitlines()
    assert json.loads(lines[0]) == [{'interval': 5, 'marker': ';'}]
    assert json.loads(lines[1]) == [{'COUNT(*)': 2}]


@pytest.mark.parametrize('timezone', ['UTC', 'Europe/Berlin'])
def test_native_loop_extrema_calendar_and_raw_series(tmp_path, monkeypatch, timezone):
    original_timezone = os.environ.get('TZ')
    monkeypatch.setenv('TZ', timezone)
    time.tzset()
    start = int(time.mktime((2024, 3, 30, 0, 0, 0, 0, 0, -1)))
    stop = int(time.mktime((2024, 4, 2, 0, 0, 0, 0, 0, -1)))
    weewx.accum.initialize({})
    managers = [weewx.manager.DaySummaryManager.open_with_create(
        {'driver': driver, 'database_name': str(tmp_path / name)},
        schema=weewx.schemas.wview_small.schema)
        for driver, name in [('weedb.sqlite', 'source.sdb'), ('weewxdb.driver', 'native.wwdb')]]
    try:
        for timestamp in range(start + 3600, stop + 1, 3600):
            accumulator = weewx.accum.Accum(weeutil.weeutil.TimeSpan(timestamp - 3600, timestamp))
            for offset, value in [(1800, -33.0), (30, 48.0)]:
                accumulator.addRecord({'dateTime': timestamp - offset,
                                       'usUnits': weewx.METRICWX, 'outTemp': value})
            for manager in managers:
                manager.addRecord({'dateTime': timestamp, 'usUnits': weewx.METRICWX,
                                   'interval': 60, 'outTemp': 0.0, 'rain': None},
                                  accumulator=accumulator, log_success=False, log_failure=False)
        for observation in ['outTemp', 'rain']:
            for aggregate, interval in [(None, None), ('avg', 10800), ('min', 86400),
                                         ('max', 86400), ('avg', 86400), ('count', 86400)]:
                expected = weewx.xtypes.get_series(observation, weeutil.weeutil.TimeSpan(start, stop), managers[0],
                                                  aggregate_type=aggregate, aggregate_interval=interval)
                actual = weewx.xtypes.get_series(observation, weeutil.weeutil.TimeSpan(start, stop), managers[1],
                                                aggregate_type=aggregate, aggregate_interval=interval)
                assert actual == expected
                if observation == 'outTemp' and aggregate == 'min':
                    assert set(actual[2][0]) == {-33.0}
    finally:
        for manager in managers:
            manager.close()
        if original_timezone is None:
            os.environ.pop('TZ', None)
        else:
            os.environ['TZ'] = original_timezone
        time.tzset()


def test_killed_writer_discards_open_transaction_but_keeps_acked_commit(tmp_path):
    path = tmp_path / 'killed.wwdb'
    weewxdb._weewxdb.create(str(path))
    connection = weewxdb.connect(path)
    connection.execute('CREATE TABLE archive(dateTime INTEGER PRIMARY KEY,value REAL)').close()
    connection.close()
    worker = subprocess.Popen([sys.executable, '-u', '-c', '''
import sys
import weewxdb
db = weewxdb.connect(sys.argv[1])
db.execute('INSERT INTO archive VALUES (300,1)').close()
print('committed', flush=True)
db.execute('BEGIN').close()
db.execute('INSERT INTO archive VALUES (600,2)').close()
print('pending', flush=True)
sys.stdin.read()
''', str(path)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
    try:
        assert worker.stdout.readline().strip() == 'committed'
        assert worker.stdout.readline().strip() == 'pending'
        worker.kill()
        worker.wait(timeout=5)
        with weewxdb.connect(path) as restored:
            assert restored.execute('SELECT * FROM archive').fetchall() == [(300, 1.0)]
            restored.execute('INSERT INTO archive VALUES (900,3)').close()
    finally:
        if worker.poll() is None:
            worker.kill()
            worker.wait(timeout=5)


def test_dbapi_comments_and_configurable_workspace(tmp_path):
    path = tmp_path / 'budgets.wwdb'
    weewxdb._weewxdb.create(str(path))
    connection = weewxdb.connect(path, cache_mib=128, workspace_mib=1, autocommit=False)
    connection.execute('CREATE TABLE archive(dateTime INTEGER PRIMARY KEY,value REAL)').close()
    connection.execute('-- enter transaction\nBEGIN IMMEDIATE').close()
    connection.execute('INSERT INTO archive VALUES (1,0),(1000000,1)').close()
    connection.rollback()
    assert connection.execute('SELECT COUNT(*) FROM archive').fetchone() == (0,)
    with connection:
        connection.execute('/* insert */ INSERT INTO archive VALUES (1,0),(1000000,1)').close()
    with pytest.raises(weewxdb.OperationalError, match='workspace'):
        connection.execute('SELECT time_bucket(dateTime,1),AVG(value) FROM archive GROUP BY 1')
    assert connection.execute('SELECT * FROM archive ORDER BY dateTime').fetchall() == [(1, 0.0), (1000000, 1.0)]
    connection.close()
