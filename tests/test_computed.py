"""Compare fused weather calculations with WeeWX's Python XTypes."""

import math
import time

import pytest

weewxdb = pytest.importorskip('weewxdb')

import weeutil.weeutil
import weewx
import weewx.manager
import weewx.units
import weewx.wxxtypes
import weewx.xtypes
from weewxdb.xtypes import NativeXType


def scalar_provider(algorithm='new'):
    return weewx.wxxtypes.WXXTypes(
        weewx.units.ValueTuple(40, 'meter', 'group_altitude'), 52, 13,
        heatindex_algo=algorithm)


def managers(tmp_path, records):
    schema = [('dateTime', 'INTEGER PRIMARY KEY'), ('usUnits', 'INTEGER NOT NULL'),
              ('interval', 'INTEGER NOT NULL'), ('outTemp', 'REAL'),
              ('outHumidity', 'REAL'), ('inTemp', 'REAL'), ('inHumidity', 'REAL')]
    result = [weewx.manager.Manager.open_with_create(
        {'driver': driver, 'database_name': str(tmp_path / filename)}, schema=schema)
        for driver, filename in [('weedb.sqlite', 'source.sdb'), ('weewxdb.driver', 'live.wwdb')]]
    for manager in result:
        manager.addRecord(records, log_success=False, log_failure=False)
    return result


def assert_value(actual, expected):
    assert actual[1:] == expected[1:]
    assert actual[0] == pytest.approx(expected[0], rel=1e-11, abs=1e-11)


@pytest.mark.parametrize('unit_system', [weewx.US, weewx.METRIC, weewx.METRICWX])
@pytest.mark.parametrize('algorithm', ['new', 'old'])
def test_computed_xtypes_match_python_without_loading_records(tmp_path, monkeypatch,
                                                              unit_system, algorithm):
    providers = [provider for provider in weewx.xtypes.xtypes
                 if not isinstance(provider, NativeXType)]
    monkeypatch.setattr(weewx.xtypes, 'xtypes', providers + [scalar_provider(algorithm)])
    start = 1704067200
    records = []
    for index in range(1, 601):
        if 181 <= index <= 192:
            continue
        celsius = 15 + 25 * math.sin(index / 19)
        temperature = celsius * 1.8 + 32 if unit_system == weewx.US else celsius
        records.append({'dateTime': start + index * 300, 'usUnits': unit_system,
                        'interval': 5, 'outTemp': None if index % 17 == 0 or 289 <= index <= 300 else temperature,
                        'outHumidity': None if index % 23 == 0 else float(index % 101),
                        'inTemp': temperature, 'inHumidity': 60.0})
    baseline, native = managers(tmp_path, records)
    native.connection.native.checkpoint()
    for manager in [baseline, native]:
        with manager.connection.cursor() as cursor:
            cursor.execute('UPDATE archive SET outHumidity=NULL WHERE dateTime=?', (start + 3000,))
    span = weeutil.weeutil.TimeSpan(start, records[-1]['dateTime'])

    def forbid_record_loading(*args, **kwargs):
        raise AssertionError('Native XType fell back to Python record processing')

    monkeypatch.setattr(native, 'genBatchRecords', forbid_record_loading)
    try:
        for observation in ['dewpoint', 'inDewpoint', 'humidex', 'heatindex']:
            expected = weewx.xtypes.get_series(observation, span, baseline)
            actual = weewx.xtypes.get_series(observation, span, native)
            assert actual[:2] == expected[:2]
            assert_value(actual[2], expected[2])
            for aggregate in ['sum', 'avg', 'min', 'max', 'count']:
                assert_value(weewx.xtypes.get_aggregate(observation, span, aggregate, native),
                             weewx.xtypes.get_aggregate(observation, span, aggregate, baseline))
            for aggregate in ['sum', 'avg', 'min', 'max', 'count', 'cumulative']:
                expected = weewx.xtypes.get_series(observation, span, baseline,
                                                  aggregate_type=aggregate, aggregate_interval=3600)
                actual = weewx.xtypes.get_series(observation, span, native,
                                                aggregate_type=aggregate, aggregate_interval=3600)
                assert actual[:2] == expected[:2]
                assert_value(actual[2], expected[2])
        # No archive rows occur in this span. XTypeTable's SUM is zero, with
        # unknown units. A span with NULL inputs still has known archive units.
        empty = weeutil.weeutil.TimeSpan(span[1] + 300, span[1] + 600)
        for aggregate in ['sum', 'avg', 'min', 'max', 'count']:
            assert_value(weewx.xtypes.get_aggregate('dewpoint', empty, aggregate, native),
                         weewx.xtypes.get_aggregate('dewpoint', empty, aggregate, baseline))
        for selected in [empty, weeutil.weeutil.TimeSpan(start, start + 192 * 300)]:
            for aggregate in [None, 'sum', 'avg', 'count']:
                expected = weewx.xtypes.get_series('dewpoint', selected, baseline,
                                                  aggregate_type=aggregate, aggregate_interval=3600)
                actual = weewx.xtypes.get_series('dewpoint', selected, native,
                                                aggregate_type=aggregate, aggregate_interval=3600)
                assert actual[:2] == expected[:2]
                assert_value(actual[2], expected[2])
    finally:
        baseline.close()
        native.close()


def test_custom_scalar_provider_and_stored_values_keep_precedence(tmp_path, monkeypatch):
    class Custom(weewx.xtypes.XType):
        def get_scalar(self, obs_type, record, db_manager=None, **options):
            if obs_type != 'dewpoint':
                raise weewx.UnknownType(obs_type)
            return weewx.units.ValueTuple(-123.0, 'degree_C', 'group_temperature')

    providers = [provider for provider in weewx.xtypes.xtypes
                 if not isinstance(provider, NativeXType)]
    monkeypatch.setattr(weewx.xtypes, 'xtypes', [Custom()] + providers + [scalar_provider()])
    records = [{'dateTime': 300, 'usUnits': 17, 'interval': 5,
                'outTemp': 30.0, 'outHumidity': 80.0}]
    baseline, native = managers(tmp_path, records)
    try:
        assert weewx.xtypes.get_aggregate('dewpoint', weeutil.weeutil.TimeSpan(0, 300), 'avg', native)[0] == -123.0
        with native.connection.cursor() as cursor:
            cursor.execute('ALTER TABLE archive ADD COLUMN dewpoint REAL DEFAULT 42.0')
        native.sqlkeys = native.connection.columnsOf('archive')
        assert weewx.xtypes.get_series('dewpoint', (0, 300), native)[2][0] == [42.0]
    finally:
        baseline.close()
        native.close()


def test_mixed_units_are_rejected_by_computed_series(tmp_path, monkeypatch):
    providers = [provider for provider in weewx.xtypes.xtypes
                 if not isinstance(provider, NativeXType)]
    monkeypatch.setattr(weewx.xtypes, 'xtypes', providers + [scalar_provider()])
    start = 1704067200
    records = [{'dateTime': start + time, 'usUnits': 17, 'interval': 5,
                'outTemp': 30.0, 'outHumidity': 80.0} for time in [300, 600]]
    baseline, native = managers(tmp_path, records)
    try:
        with native.connection.cursor() as cursor:
            cursor.execute('UPDATE archive SET usUnits=1 WHERE dateTime=?', (start + 600,))
        for aggregate in [None, 'avg']:
            with pytest.raises(weewx.UnsupportedFeature, match='unit system changes'):
                weewx.xtypes.get_series('dewpoint', (start, start + 600), native,
                                        aggregate_type=aggregate, aggregate_interval=600)
    finally:
        baseline.close()
        native.close()


def test_custom_aggregate_provider_keeps_precedence(tmp_path, monkeypatch):
    class Custom(weewx.xtypes.XType):
        def get_aggregate(self, obs_type, timespan, aggregate_type, db_manager, **options):
            if obs_type != 'dewpoint':
                raise weewx.UnknownType(obs_type)
            return weewx.units.ValueTuple(-456.0, 'degree_C', 'group_temperature')

    providers = [provider for provider in weewx.xtypes.xtypes
                 if not isinstance(provider, NativeXType)]
    monkeypatch.setattr(weewx.xtypes, 'xtypes', [Custom()] + providers + [scalar_provider()])
    baseline, native = managers(tmp_path, [{'dateTime': 300, 'usUnits': 17, 'interval': 5,
                                           'outTemp': 30.0, 'outHumidity': 80.0}])
    try:
        assert weewx.xtypes.get_aggregate('dewpoint', (0, 300), 'avg', native)[0] == -456.0
    finally:
        baseline.close()
        native.close()


@pytest.mark.parametrize('month,day,hours', [(3, 30, 23), (10, 26, 25)])
def test_computed_calendar_bins_follow_dst(tmp_path, monkeypatch, month, day, hours):
    providers = [provider for provider in weewx.xtypes.xtypes
                 if not isinstance(provider, NativeXType)]
    monkeypatch.setattr(weewx.xtypes, 'xtypes', providers + [scalar_provider()])
    try:
        with monkeypatch.context() as scoped:
            scoped.setenv('TZ', 'Europe/Berlin')
            time.tzset()
            start = int(time.mktime((2024, month, day, 0, 0, 0, 0, 0, -1)))
            stop = int(time.mktime((2024, month, day + 3, 0, 0, 0, 0, 0, -1)))
            records = [{'dateTime': stamp, 'usUnits': 17, 'interval': 60,
                        'outTemp': float(index % 21), 'outHumidity': 80.0}
                       for index, stamp in enumerate(range(start + 3600, stop + 1, 3600))]
            baseline, native = managers(tmp_path, records)
            try:
                for interval in [86400, 'month']:
                    expected = weewx.xtypes.get_series('humidex', (start, stop), baseline,
                                                      aggregate_type='avg', aggregate_interval=interval)
                    actual = weewx.xtypes.get_series('humidex', (start, stop), native,
                                                    aggregate_type='avg', aggregate_interval=interval)
                    assert actual[:2] == expected[:2]
                    assert_value(actual[2], expected[2])
                    if interval == 86400:
                        assert (actual[1][0][1] - actual[0][0][1]) / 3600 == hours
            finally:
                baseline.close()
                native.close()
    finally:
        time.tzset()
