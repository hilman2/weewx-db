"""Differential checks against unmodified WeeWX and the pinned xaggs extension."""

import contextlib
import importlib.util
import math
import os
import pathlib
import sys
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


@pytest.fixture(scope='module', autouse=True)
def utc_timezone():
    previous = os.environ.get('TZ')
    os.environ['TZ'] = 'UTC'
    time.tzset()
    try:
        yield
    finally:
        if previous is None:
            os.environ.pop('TZ', None)
        else:
            os.environ['TZ'] = previous
        time.tzset()


def builtin_providers(algorithm='new', solar='rs', altimeter='aaASOS'):
    altitude = weewx.units.ValueTuple(155, 'meter', 'group_altitude')
    return [NativeXType(), weewx.xtypes.WindVecDaily(), weewx.xtypes.WindVec(),
            weewx.xtypes.AggregateHeatCool(), weewx.xtypes.DailySummaries(),
            weewx.xtypes.ArchiveTable(), weewx.xtypes.XTypeTable(),
            weewx.wxxtypes.WXXTypes(altitude, 52, 13, heatindex_algo=algorithm,
                                     maxSolarRad_algo=solar, atc=0.82, nfac=3),
            weewx.wxxtypes.ETXType(altitude, 52, 13, et_period=5400, wind_height=3.1,
                                    albedo=0.25, cn=40, cd=0.38),
            weewx.wxxtypes.PressureCooker(altitude, max_delta_12h=1800,
                                           altimeter_algorithm=altimeter),
            weewx.wxxtypes.RainRater(rain_period=900, retain_period=1800),
            weewx.wxxtypes.Delta({'deltaRain': {'input': 'totalRain'}})]


def compare(actual, expected):
    if isinstance(expected, (tuple, list)):
        assert len(actual) == len(expected)
        for a, b in zip(actual, expected):
            compare(a, b)
    elif isinstance(expected, (float, complex)):
        assert actual == pytest.approx(expected, rel=1e-10, abs=1e-10)
    else:
        assert actual == expected


@contextlib.contextmanager
def forbid_python_weather():
    previous = sys.getprofile()

    def profile(frame, event, argument):
        if event == 'call' and frame.f_code.co_filename.endswith(('/weewx/wxformulas.py', '/weewx/wxxtypes.py')):
            raise AssertionError('Native calculation used Python weather code: %s' % frame.f_code.co_name)

    sys.setprofile(profile)
    try:
        yield
    finally:
        sys.setprofile(previous)


def hardware_records(start, units, count=100, interval=1800):
    result = []
    for index in range(1, count + 1):
        celsius = 12 + 25 * math.sin(index / 9)
        record = {'dateTime': start + index * interval, 'usUnits': 17,
                  'interval': interval // 60, 'outTemp': celsius, 'inTemp': 23.0,
                  'outHumidity': float(30 + index % 70), 'inHumidity': 75.0,
                  'windSpeed': float(index % 8), 'windDir': float(index * 11 % 360),
                  'windGust': float(index % 8 + 4), 'windGustDir': float(index * 13 % 360),
                  'radiation': float(max(0, 500 * math.sin(index / 9))),
                  'pressure': 990.0, 'barometer': 1012.0,
                  'rain': 0.2 if index % 3 == 0 else 0.0, 'totalRain': float(index // 3)}
        result.append(weewx.units.to_std_system(record, units))
    return result


def open_pair(tmp_path, records, daily=False):
    names = list(records[0])
    schema = [(name, 'INTEGER PRIMARY KEY' if name == 'dateTime' else
               'INTEGER NOT NULL' if name in ['usUnits', 'interval'] else 'REAL') for name in names]
    if daily:
        schema = {'table': schema,
                  'day_summaries': [(name, 'scalar') for name in names if name not in ['dateTime', 'usUnits', 'interval']] + [('wind', 'VECTOR')]}
    kind = weewx.manager.DaySummaryManager if daily else weewx.manager.Manager
    managers = [kind.open_with_create({'driver': driver, 'database_name': str(tmp_path / filename)}, schema=schema)
                for driver, filename in [('weedb.sqlite', 'source.sdb'), ('weewxdb.driver', 'live.wwdb')]]
    for manager in managers:
        manager.addRecord(records, log_success=False, log_failure=False)
    managers[1].connection.native.checkpoint()
    return managers


@pytest.mark.parametrize('units', [1, 16, 17])
@pytest.mark.parametrize('solar', ['rs', 'bras'])
def test_every_scalar_kernel_matches_builtin_without_python_weather(tmp_path, monkeypatch, units, solar):
    monkeypatch.setitem(weewx.units.obs_group_dict, 'deltaRain', 'group_rain')
    records = hardware_records(1704067200, units)
    baseline, native = open_pair(tmp_path, records)
    monkeypatch.setattr(weewx.almanac, 'almanacs', [weewx.almanac.PyEphemAlmanacType()])
    groups = [builtin_providers(solar=solar), builtin_providers(solar=solar)]
    observations = ['dewpoint', 'inDewpoint', 'heatindex', 'humidex', 'windchill',
                    'appTemp', 'cloudbase', 'windrun', 'beaufort', 'maxSolarRad',
                    'pressure', 'altimeter', 'barometer', 'ET', 'rainRate', 'deltaRain',
                    'windDir', 'windGustDir']
    try:
        for manager in [native]:
            for name in ['genBatchRecords', 'getRecord', 'getSql', 'genSql']:
                monkeypatch.setattr(manager, name, lambda *a, **k: pytest.fail('Python database access used'))
        for observation in observations:
            for record in [records[30], records[31], records[50], records[70]]:
                monkeypatch.setattr(weewx.xtypes, 'xtypes', groups[0])
                try:
                    expected = weewx.xtypes.get_scalar(observation, record, baseline)
                except weewx.NoCalculate:
                    monkeypatch.setattr(weewx.xtypes, 'xtypes', groups[1])
                    with forbid_python_weather(), pytest.raises(weewx.NoCalculate):
                        weewx.xtypes.get_scalar(observation, record, native)
                else:
                    monkeypatch.setattr(weewx.xtypes, 'xtypes', groups[1])
                    with forbid_python_weather():
                        actual = weewx.xtypes.get_scalar(observation, record, native)
                    assert actual[0] is not None or expected[0] is None, (observation, actual, expected)
                    compare(actual, expected)
    finally:
        baseline.close()
        native.close()


@pytest.mark.parametrize('algorithm', ['aaASOS', 'aaASOS2', 'aaMADIS', 'aaNOAA', 'aaWOB', 'aaSMT'])
@pytest.mark.parametrize('units', [1, 17])
def test_altimeter_algorithms(tmp_path, monkeypatch, algorithm, units):
    records = hardware_records(1704067200, units, count=2)
    baseline, native = open_pair(tmp_path, records)
    monkeypatch.setattr(weewx.xtypes, 'xtypes', builtin_providers(altimeter=algorithm))
    try:
        expected = weewx.xtypes.get_scalar('altimeter', records[0], baseline)
        with forbid_python_weather():
            actual = weewx.xtypes.get_scalar('altimeter', records[0], native)
        compare(actual, expected)
    finally:
        baseline.close()
        native.close()


@pytest.fixture(scope='module')
def daily_pair(tmp_path_factory):
    start = 1704067200
    records = hardware_records(start, 17, count=360, interval=3600)
    for index, record in enumerate(records):
        if index % 17 == 0:
            record['outTemp'] = None
        if index % 19 == 0:
            record['windDir'] = None
    pair = open_pair(tmp_path_factory.mktemp('all-daily'), records, daily=True)
    try:
        yield pair, weeutil.weeutil.TimeSpan(start, records[-1]['dateTime'])
    finally:
        for manager in pair:
            manager.close()


@pytest.mark.parametrize('aggregate', list(weewx.xtypes.DailySummaries.agg_sql_dict))
def test_every_daily_aggregate_matches_builtin(daily_pair, monkeypatch, aggregate):
    (baseline, native), span = daily_pair
    monkeypatch.setattr(weewx.xtypes, 'xtypes', builtin_providers())
    observation = 'wind' if aggregate in ['rms', 'vecavg', 'vecdir', 'gustdir'] else 'outTemp'
    options = {'val': (12.0, 'degree_C', 'group_temperature')}
    expected = weewx.xtypes.get_aggregate(observation, span, aggregate, baseline, **options)
    for name in ['genBatchRecords', 'getRecord', 'getSql', 'genSql']:
        monkeypatch.setattr(native, name, lambda *a, **k: pytest.fail('Python database access used'))
    actual = weewx.xtypes.get_aggregate(observation, span, aggregate, native, **options)
    compare(actual, expected)


@pytest.mark.parametrize('observation', ['windvec', 'windgustvec'])
@pytest.mark.parametrize('aggregate', ['sum', 'avg', 'min', 'max', 'first', 'last', 'count', 'not_null'])
def test_every_wind_vector_aggregate(daily_pair, monkeypatch, observation, aggregate):
    (baseline, native), full_span = daily_pair
    span = weeutil.weeutil.TimeSpan(full_span.start + 5, full_span.stop - 5)
    monkeypatch.setattr(weewx.xtypes, 'xtypes', builtin_providers())
    expected = weewx.xtypes.get_aggregate(observation, span, aggregate, baseline)
    for name in ['genBatchRecords', 'getRecord', 'getSql', 'genSql']:
        monkeypatch.setattr(native, name, lambda *a, **k: pytest.fail('Python database access used'))
    actual = weewx.xtypes.get_aggregate(observation, span, aggregate, native)
    compare(actual, expected)


@pytest.mark.parametrize('observation', ['windvec', 'outTemp'])
def test_raw_series_over_whole_days(daily_pair, monkeypatch, observation):
    # Whole days qualify for the daily summaries, which hold no archive records.
    from weewxdb.batch import get_series_many
    (baseline, native), full_span = daily_pair
    span = weeutil.weeutil.TimeSpan(full_span.start + 86400, full_span.start + 3 * 86400)
    monkeypatch.setattr(weewx.xtypes, 'xtypes', builtin_providers())
    expected = weewx.xtypes.get_series(observation, span, baseline)
    request = {'observation': observation, 'span': span, 'aggregate': None, 'interval': None}
    compare(get_series_many([request], native)[0], expected)


@pytest.fixture
def xaggs(monkeypatch):
    path = pathlib.Path(__file__).resolve().parents[1] / 'build/weewx-db-trial/xaggs-source/bin/user/xaggs.py'
    if not path.is_file():
        pytest.skip('Fetch the pinned weewx-xaggs reference for differential tests')
    spec = importlib.util.spec_from_file_location('reference_xaggs', path)
    module = importlib.util.module_from_spec(spec)
    monkeypatch.setitem(sys.modules, spec.name, module)
    spec.loader.exec_module(module)
    monkeypatch.setattr(weewx.xtypes, 'xtypes', builtin_providers() + [module.XAggsHistorical(), module.XAggsAvg()])
    return module


@pytest.mark.parametrize('aggregate', ['historical_min', 'historical_max', 'historical_min_avg',
                                       'historical_max_avg', 'historical_mintime', 'historical_maxtime',
                                       'historical_avg', 'avg_ge', 'avg_gt', 'avg_le', 'avg_lt'])
def test_all_xaggs_aggregates(daily_pair, monkeypatch, xaggs, aggregate):
    (baseline, native), full_span = daily_pair
    if aggregate.startswith('historical_'):
        span = weeutil.weeutil.TimeSpan(full_span.start, full_span.start + 86400)
    else:
        span = weeutil.weeutil.TimeSpan(full_span.start + 123, full_span.stop - 123)
    options = {'val': (12.0, 'degree_C', 'group_temperature')}
    expected = weewx.xtypes.get_aggregate('outTemp', span, aggregate, baseline, **options)
    for name in ['genBatchRecords', 'getRecord', 'getSql', 'genSql']:
        monkeypatch.setattr(native, name, lambda *a, **k: pytest.fail('Python database access used'))
    actual = weewx.xtypes.get_aggregate('outTemp', span, aggregate, native, **options)
    compare(actual, expected)


@pytest.mark.parametrize('observation', ['windchill', 'appTemp', 'cloudbase', 'windrun', 'beaufort',
                                        'maxSolarRad', 'ET', 'pressure', 'rainRate', 'deltaRain'])
@pytest.mark.parametrize('aggregate', [None, 'sum', 'avg', 'min', 'max', 'count', 'cumulative'])
def test_computed_series_uses_one_native_request(tmp_path, monkeypatch, observation, aggregate):
    monkeypatch.setitem(weewx.units.obs_group_dict, 'deltaRain', 'group_rain')
    records = hardware_records(1704067200, 17, count=90)
    if observation == 'pressure':
        for record in records:
            del record['pressure']
    baseline, native = open_pair(tmp_path, records)
    monkeypatch.setattr(weewx.almanac, 'almanacs', [weewx.almanac.PyEphemAlmanacType()])
    span = weeutil.weeutil.TimeSpan(records[0]['dateTime'] - 1800, records[-1]['dateTime'])
    options = dict(aggregate_type=aggregate, aggregate_interval=10800)
    try:
        monkeypatch.setattr(weewx.xtypes, 'xtypes', builtin_providers())
        expected = weewx.xtypes.get_series(observation, span, baseline, **options)
        monkeypatch.setattr(weewx.xtypes, 'xtypes', builtin_providers())
        for name in ['genBatchRecords', 'getRecord', 'getSql', 'genSql']:
            monkeypatch.setattr(native, name, lambda *a, **k: pytest.fail('Python database access used'))

        class CountRequests:
            def __init__(self, original):
                self.original = original
                self.count = 0

            def analytics_native(self, specification):
                self.count += 1
                return self.original.analytics_native(specification)

            def __getattr__(self, name):
                return getattr(self.original, name)

        counter = CountRequests(native.connection.native)
        monkeypatch.setattr(native.connection, 'native', counter)
        with forbid_python_weather():
            actual = weewx.xtypes.get_series(observation, span, native, **options)
        compare(actual, expected)
        assert counter.count == 1
    finally:
        baseline.close()
        native.close()


@pytest.mark.parametrize('aggregate', list(weewx.xtypes.ArchiveTable.valid_aggregate_types))
def test_every_archive_aggregate(daily_pair, monkeypatch, aggregate):
    (baseline, native), full_span = daily_pair
    span = weeutil.weeutil.TimeSpan(full_span.start + 17, full_span.stop - 17)
    monkeypatch.setattr(weewx.xtypes, 'xtypes', builtin_providers())
    observation = 'wind' if aggregate in ['vecavg', 'vecdir', 'gustdir'] else 'outTemp'
    expected = weewx.xtypes.get_aggregate(observation, span, aggregate, baseline)
    for name in ['genBatchRecords', 'getRecord', 'getSql', 'genSql']:
        monkeypatch.setattr(native, name, lambda *a, **k: pytest.fail('Python database access used'))
    actual = weewx.xtypes.get_aggregate(observation, span, aggregate, native)
    compare(actual, expected)


@pytest.mark.parametrize('observation', ['heatdeg', 'cooldeg', 'growdeg'])
@pytest.mark.parametrize('aggregate', ['sum', 'avg', 'not_null'])
def test_degree_days(daily_pair, monkeypatch, observation, aggregate):
    (baseline, native), span = daily_pair
    monkeypatch.setattr(weewx.xtypes, 'xtypes', builtin_providers())
    options = {'skin_dict': {'Units': {'DegreeDays': {'heating_base': (14.5, 'degree_C'),
                                                      'cooling_base': (22.0, 'degree_C'),
                                                      'growing_base': (8.0, 'degree_C')}}}}
    expected = weewx.xtypes.get_aggregate(observation, span, aggregate, baseline, **options)
    for name in ['genBatchRecords', 'getRecord', 'getSql', 'genSql']:
        monkeypatch.setattr(native, name, lambda *a, **k: pytest.fail('Python database access used'))
    actual = weewx.xtypes.get_aggregate(observation, span, aggregate, native, **options)
    compare(actual, expected)
