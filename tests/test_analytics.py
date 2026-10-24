"""Exercise statistics, bounded workspaces and state across native queries."""

import json
import math
import statistics

import pytest

import weeutil.weeutil
import weewx
import weewx.units
import weewx.wxxtypes
import weewx.xtypes
import weewxdb

from test_all_xtypes import builtin_providers, compare, hardware_records, open_pair


@pytest.fixture
def pair(tmp_path, monkeypatch):
    monkeypatch.setattr(weewx.xtypes, 'xtypes', builtin_providers())
    records = hardware_records(1704067200, 17, count=160, interval=3600)
    for index, record in enumerate(records):
        if index % 11 == 0:
            record['outTemp'] = None
    managers = open_pair(tmp_path, records, daily=True)
    try:
        yield managers, records
    finally:
        for manager in managers:
            manager.close()


@pytest.mark.parametrize('aggregate', ['median', 'percentile', 'stddev', 'stddev_samp',
                                       'variance', 'rms', 'slope', 'weighted_avg', 'integral',
                                       'maxgap', 'longest_ge', 'longest_lt'])
def test_additional_statistics(pair, aggregate):
    (baseline, native), records = pair
    span = (records[0]['dateTime'] - 3600 + 1, records[-1]['dateTime'])
    valid = [r for r in records if r['outTemp'] is not None]
    values = [r['outTemp'] for r in valid]
    fraction = 0.83
    position = fraction * (len(values) - 1)
    ordered = sorted(values)
    mean_x = statistics.mean(r['dateTime'] - records[0]['dateTime'] for r in valid)
    mean_y = statistics.mean(values)
    denominator = sum((r['dateTime'] - records[0]['dateTime'] - mean_x) ** 2 for r in valid)
    slope = sum((r['dateTime'] - records[0]['dateTime'] - mean_x) * (r['outTemp'] - mean_y) for r in valid) / denominator
    streak, longest = 0, 0
    for record in records:
        value = record['outTemp']
        matches = value is not None and (value >= 15 if aggregate == 'longest_ge' else value < 15)
        streak = streak + 1 if matches else 0
        longest = max(streak, longest)
    expected = {'median': statistics.median(values),
                'percentile': ordered[math.floor(position)] + (ordered[math.ceil(position)] - ordered[math.floor(position)]) * (position % 1),
                'stddev': statistics.pstdev(values), 'stddev_samp': statistics.stdev(values),
                'variance': statistics.pvariance(values), 'rms': math.sqrt(statistics.mean(v * v for v in values)),
                'slope': slope, 'weighted_avg': statistics.mean(values), 'integral': sum(values) * 3600,
                'maxgap': 3600, 'longest_ge': longest, 'longest_lt': longest}[aggregate]
    result = weewx.xtypes.get_aggregate('outTemp', span, aggregate, native,
                                       fraction=fraction, val=(15, 'degree_C', 'group_temperature'))
    assert result[0] == pytest.approx(expected, abs=1e-8, rel=1e-11)


@pytest.mark.parametrize('aggregate', ['min', 'max', 'avg', 'sum', 'count'])
@pytest.mark.parametrize('interval', [86400, 'month', 'year'])
def test_calendar_series_preserve_daily_labels(pair, aggregate, interval):
    (baseline, native), records = pair
    span = weeutil.weeutil.TimeSpan(records[0]['dateTime'] - 3600, records[-1]['dateTime'])
    expected = weewx.xtypes.get_series('outTemp', span, baseline, aggregate_type=aggregate, aggregate_interval=interval)
    actual = weewx.xtypes.get_series('outTemp', span, native, aggregate_type=aggregate, aggregate_interval=interval)
    compare(actual, expected)


def test_et_missing_window_returns_unspecified_units(pair):
    (baseline, native), records = pair
    record = dict(records[0], dateTime=records[0]['dateTime'] - 86400)
    expected = weewx.xtypes.get_scalar('ET', record, baseline)
    actual = weewx.xtypes.get_scalar('ET', record, native)
    assert actual == expected == (None, None, None)


@pytest.mark.parametrize('reverse', [False, True])
def test_rain_fractional_times_and_small_tip_after_large_old_tip(pair, monkeypatch, reverse):
    (baseline, native), records = pair
    stamp = records[0]['dateTime'] + 1000.5
    rain = weewx.wxxtypes.RainRater(rain_period=900.25, retain_period=1800)
    rain.augmented = True
    rain.unit_system = 17
    rain.rain_events = [(stamp - 1000, 1e16), (stamp - 900.25, 1e15),
                        (stamp - 900.0, 0.1), (stamp - 1.5, 0.2)]
    if reverse:
        rain.rain_events.reverse()
    original = rain.rain_events[:]
    monkeypatch.setattr(weewx.xtypes, 'xtypes', [weewxdb.xtypes.NativeXType(), rain])
    record = dict(records[0], dateTime=stamp)
    expected = weewx.xtypes.get_scalar('rainRate', record, baseline)
    actual = weewx.xtypes.get_scalar('rainRate', record, native)
    compare(actual, expected)
    assert actual[0] == pytest.approx(3600 * 0.3 / 900.25)
    assert rain.rain_events == original


def test_custom_series_and_aggregate_priority(pair, monkeypatch):
    (baseline, native), records = pair

    class Custom(weewx.xtypes.XType):
        def get_series(self, *args, **kwargs):
            return 'custom series'

        def get_aggregate(self, *args, **kwargs):
            return 'custom aggregate'

    providers = builtin_providers()
    providers.insert(1, Custom())
    monkeypatch.setattr(weewx.xtypes, 'xtypes', providers)
    span = (records[0]['dateTime'], records[-1]['dateTime'])
    assert weewx.xtypes.get_series('outTemp', span, native, aggregate_type='avg', aggregate_interval=3600) == 'custom series'
    assert weewx.xtypes.get_aggregate('outTemp', span, 'avg', native) == 'custom aggregate'


def test_percentile_memory_is_shared_by_all_bins(tmp_path):
    path = tmp_path / 'limited.wwdb'
    weewxdb._weewxdb.create(str(path))
    with weewxdb.connect(path, workspace_mib=1) as connection:
        connection.execute('CREATE TABLE archive(dateTime INTEGER PRIMARY KEY, usUnits INTEGER, interval INTEGER, outTemp REAL)').close()
        connection.executemany('INSERT INTO archive VALUES (?,17,1,?)', [(i, float(i % 33)) for i in range(40000)]).close()
        query = {'table': 'archive', 'mode': 'archive', 'observation': 'outTemp', 'aggregate': 'median',
                 'ranges': [[-1, 19999], [19999, 39999]]}
        with pytest.raises(ValueError, match='exact percentile workspace exceeded'):
            connection.native.analytics(json.dumps(query))
        query.update(aggregate='avg')
        assert len(json.loads(connection.native.analytics(json.dumps(query)))['values']) == 2
        query.update(ranges=[[10, 30], [20, 40]])
        with pytest.raises(ValueError, match='ordered and disjoint'):
            connection.native.analytics(json.dumps(query))


def test_sql_weather_and_astronomy(pair):
    (baseline, native), records = pair
    row = native.getSql("SELECT wx_value('windchill',dateTime,usUnits,'{}',outTemp,windSpeed) FROM archive WHERE dateTime=?", (records[1]['dateTime'],))
    expected = weewx.xtypes.get_scalar('windchill', records[1], baseline)
    assert row[0] == pytest.approx(expected[0])
    row = native.getSql("SELECT json_extract(astro_position('sun',?,52,13,0),'$.altitude')", (1704110400,))
    assert row[0] == pytest.approx(weewxdb.astronomy(1704110400, 52, 13)['altitude'])
    row = native.getSql("SELECT astro_events('sun',?,?,52,13,0,-18,1)", (1704067200, 1704153600))
    result = json.loads(row[0])
    assert len(result['rises']) == len(result['sets']) == 1


def test_stored_integer_xtype_preserves_values_above_float_precision(tmp_path, monkeypatch):
    import weewx.manager
    monkeypatch.setattr(weewx.xtypes, 'xtypes', builtin_providers())
    schema = [('dateTime', 'INTEGER PRIMARY KEY'), ('usUnits', 'INTEGER'),
              ('interval', 'INTEGER'), ('outTemp', 'INTEGER')]
    managers = [weewx.manager.Manager.open_with_create(
        {'driver': driver, 'database_name': str(tmp_path / name)}, schema=schema)
        for driver, name in [('weedb.sqlite', 'integers.sdb'), ('weewxdb.driver', 'integers.wwdb')]]
    records = [{'dateTime': 300 * i, 'usUnits': 17, 'interval': 5, 'outTemp': value}
               for i, value in enumerate([2 ** 53 + 1, 2 ** 53 + 3, -(2 ** 53)], 1)]
    try:
        for manager in managers:
            manager.addRecord(records, log_success=False, log_failure=False)
        for aggregate in ['min', 'max', 'sum', 'avg', 'count']:
            assert weewx.xtypes.get_aggregate('outTemp', weeutil.weeutil.TimeSpan(0, 900), aggregate, managers[0]) \
                == weewx.xtypes.get_aggregate('outTemp', weeutil.weeutil.TimeSpan(0, 900), aggregate, managers[1])
    finally:
        for manager in managers:
            manager.close()
