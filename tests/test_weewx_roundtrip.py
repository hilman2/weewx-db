"""Round-trip a native WeeWX archive through the experimental Rust format."""

import json
import os
import pathlib
import subprocess
import time

import pytest

import weeutil.weeutil
import weewx.accum
import weewx.manager
import weewx.schemas.wview_extended
import weewx.xtypes


@pytest.fixture(params=['UTC', 'Europe/Berlin'])
def timezone(request, monkeypatch):
    if not hasattr(time, 'tzset'):
        pytest.skip('Test requires time.tzset')
    try:
        with monkeypatch.context() as scoped:
            scoped.setenv('TZ', request.param)
            time.tzset()
            yield request.param
    finally:
        time.tzset()


def test_native_manager_roundtrip(tmp_path, timezone):
    binary = pathlib.Path(os.environ.get(
        'WEEWX_DB_BINARY',
        pathlib.Path(__file__).resolve().parents[1] / 'target/release/weewx-db'))
    if not binary.is_file():
        pytest.skip('Build the Rust release binary before this integration test')
    source = tmp_path / 'source.sdb'
    snapshot = tmp_path / 'snapshot.wwdb'
    restored = tmp_path / 'restored.sdb'
    source_dict = {'driver': 'weedb.sqlite', 'database_name': str(source)}
    restored_dict = {'driver': 'weedb.sqlite', 'database_name': str(restored)}
    start = int(time.mktime((2024, 3, 30, 0, 0, 0, 0, 0, -1)))
    stop = int(time.mktime((2024, 4, 2, 0, 0, 0, 0, 0, -1)))
    span = weeutil.weeutil.TimeSpan(start, stop)
    weewx.accum.initialize({})
    with weewx.manager.DaySummaryManager.open_with_create(
            source_dict, schema=weewx.schemas.wview_extended.schema) as manager:
        for index, timestamp in enumerate(range(start + 3600, stop + 1, 3600)):
            accumulator = weewx.accum.Accum(
                weeutil.weeutil.TimeSpan(timestamp - 3600, timestamp))
            for offset, temperature in [(1800, -33.0), (30, 48.0)]:
                accumulator.addRecord({
                    'dateTime': timestamp - offset, 'usUnits': weewx.METRICWX,
                    'outTemp': temperature, 'windSpeed': 10.0, 'windDir': 270.0,
                    'windGust': 15.0, 'windGustDir': 180.0})
            manager.addRecord({
                'dateTime': timestamp, 'usUnits': weewx.METRICWX, 'interval': 60,
                'outTemp': None if index % 7 == 0 else float(index % 8 - 4),
                'rain': 0.5 if index % 11 == 0 else 0.0,
                'windSpeed': 2.0, 'windDir': 90.0,
                'windGust': 3.0, 'windGustDir': 180.0},
                accumulator=accumulator, log_success=False, log_failure=False)
        expected_records = list(manager.genBatchRecords())
        expected = {}
        for observation in ['outTemp', 'rain', 'windSpeed', 'windvec']:
            aggregates = ['avg'] if observation == 'windvec' else [
                'avg', 'min', 'max', 'mintime', 'maxtime', 'sum', 'count']
            for aggregate in aggregates:
                expected[observation, aggregate] = weewx.xtypes.get_aggregate(
                    observation, span, aggregate, manager)
        assert expected['outTemp', 'min'][0] == -33.0
        assert expected['outTemp', 'max'][0] == 48.0
        expected_series = weewx.xtypes.get_series(
            'outTemp', span, manager, aggregate_type='avg', aggregate_interval=86400)
        raw_stats = {
            aggregate: weewx.xtypes.ArchiveTable.get_aggregate(
                'outTemp', span, aggregate, manager)[0]
            for aggregate in ['count', 'sum', 'min', 'max']}

    subprocess.run([str(binary), 'import', str(source), str(snapshot)],
                   check=True, capture_output=True, text=True)
    result = subprocess.run([
        str(binary), 'query', str(snapshot), 'outTemp', str(start), str(stop),
        '--cache-mib', '0'], check=True, capture_output=True, text=True)
    assert json.loads(result.stdout) == raw_stats
    subprocess.run([str(binary), 'export', str(snapshot), str(restored)],
                   check=True, capture_output=True, text=True)
    with weewx.manager.DaySummaryManager.open(restored_dict) as manager:
        assert list(manager.genBatchRecords()) == expected_records
        for (observation, aggregate), value in expected.items():
            assert weewx.xtypes.get_aggregate(
                observation, span, aggregate, manager) == value
        assert weewx.xtypes.get_series(
            'outTemp', span, manager, aggregate_type='avg',
            aggregate_interval=86400) == expected_series
