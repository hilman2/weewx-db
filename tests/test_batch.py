"""Compare fused queries against WeeWX, including policies and cache invalidation."""

import copy
import math

import pytest
import weeutil.weeutil
import weewx
import weewx.units
import weewx.xtypes
from weewxdb.batch import get_series_many, get_aggregates_many, ReportCache
from test_all_xtypes import builtin_providers, compare, hardware_records, open_pair, forbid_python_weather


def requests_for(observations, span, interval=7200, options=None):
    return [{'observation': obs, 'span': span, 'aggregate': agg, 'interval': interval,
             'options': options or {}} for obs in observations for agg in ['avg', 'min', 'max']]


@pytest.mark.parametrize('units', [1, 16, 17])
def test_batch_matches_independent_weewx_queries_one_scan(tmp_path, monkeypatch, units):
    records = hardware_records(1704067200, units, count=100)
    records[4]['outHumidity'] = None
    records[5]['windSpeed'] = None
    baseline, manager = open_pair(tmp_path, records)
    span = weeutil.weeutil.TimeSpan(records[0]['dateTime'] - 1800, records[-1]['dateTime'])
    requests = requests_for(['appTemp', 'windchill', 'dewpoint'], span)
    try:
        expected = []
        for r in requests:
            monkeypatch.setattr(weewx.xtypes, 'xtypes', builtin_providers())
            expected.append(weewx.xtypes.get_series(r['observation'], span, baseline, r['aggregate'], r['interval']))
        monkeypatch.setattr(weewx.xtypes, 'xtypes', builtin_providers())
        statistics = {}
        with forbid_python_weather():
            actual = get_series_many(requests, manager, statistics=statistics)
        compare(actual, expected)
        assert statistics == {'scans': 1, 'formula_evaluations': 3 * len(records)}
    finally:
        baseline.close()
        manager.close()


@pytest.mark.parametrize('policy', ['auto', 'stored', 'derived', 'fill_missing'])
@pytest.mark.parametrize('units', [1, 16, 17])
def test_existing_columns_and_nulls_respect_source_policy(tmp_path, monkeypatch, policy, units):
    records = hardware_records(1704067200, units, count=20)
    for i, record in enumerate(records):
        record['appTemp'] = 999.0 if i % 2 else None
    baseline, manager = open_pair(tmp_path, records)
    span = weeutil.weeutil.TimeSpan(records[0]['dateTime'] - 1800, records[-1]['dateTime'])
    try:
        monkeypatch.setattr(weewx.xtypes, 'xtypes', builtin_providers())
        derived = [weewx.wxformulas.apptempF(r['outTemp'], r['outHumidity'], r['windSpeed']) if units == 1 else
                   weewx.wxformulas.apptempC(r['outTemp'], r['outHumidity'], r['windSpeed'] if units == 17 else r['windSpeed'] / 3.6)
                   for r in records]
        source = [r['appTemp'] if policy in ['stored', 'auto'] or policy == 'fill_missing' and r['appTemp'] is not None
                  else d for r, d in zip(records, derived)]
        values = [v for v in source if v is not None]
        actual = get_aggregates_many('appTemp', span, ['avg', 'min', 'max'], manager, source=policy)
        compare([v[0] for v in actual], [sum(values) / len(values), min(values), max(values)])
    finally:
        baseline.close()
        manager.close()


def test_cache_tracks_corrections_other_connections_and_transactions(tmp_path, monkeypatch):
    records = hardware_records(1704067200, 17, count=10)
    baseline, manager = open_pair(tmp_path, records)
    span = weeutil.weeutil.TimeSpan(records[0]['dateTime'] - 1800, records[-1]['dateTime'])
    requests = requests_for(['appTemp'], span)
    cache = ReportCache(65536)
    try:
        monkeypatch.setattr(weewx.xtypes, 'xtypes', builtin_providers())
        original = get_series_many(requests, manager, cache)
        hit_statistics = {}
        again = get_series_many(requests, manager, cache, statistics=hit_statistics)
        assert hit_statistics == {}, 'Expected cached results without another native scan'
        compare(original, again)
        again[0][2][0][0] = -999
        compare(original, get_series_many(requests, manager, cache))
        assert cache.bytes <= cache.max_bytes
        import weewxdb
        with weewxdb.connect(manager.connection.file_path) as writer:
            writer.execute('UPDATE archive SET outTemp=80').close()
        changed = get_series_many(requests, manager, cache)
        assert changed != original
        manager.connection.begin()
        manager.connection.connection.execute('UPDATE archive SET outTemp=-40').close()
        uncommitted = get_series_many(requests, manager, cache)
        assert uncommitted != changed
        assert not cache.entries
        manager.connection.rollback()
        compare(get_series_many(requests, manager, cache), changed)
    finally:
        baseline.close()
        manager.close()


def test_custom_provider_before_cache_retains_priority(tmp_path, monkeypatch):
    records = hardware_records(1704067200, 17, count=10)
    baseline, manager = open_pair(tmp_path, records)
    span = weeutil.weeutil.TimeSpan(records[0]['dateTime'] - 1800, records[-1]['dateTime'])
    requests = requests_for(['appTemp'], span)
    class Custom(weewx.xtypes.XType):
        def get_series(self, obs_type, timespan, db_manager, *args, **kwargs):
            return (None, None, 'custom')
    try:
        monkeypatch.setattr(weewx.xtypes, 'xtypes', builtin_providers())
        cache = ReportCache()
        get_series_many(requests, manager, cache)
        weewx.xtypes.xtypes.insert(0, Custom())
        assert get_series_many(requests, manager, cache) == [(None, None, 'custom')] * 3
    finally:
        baseline.close()
        manager.close()


@pytest.mark.parametrize('units', [1, 16, 17])
def test_packet_batch_preserves_dependencies_missing_values_and_state(tmp_path, monkeypatch, units):
    from weewxdb.services import StdWXCalculate
    from weewx.wxservices import StdWXCalculate as Baseline
    records = hardware_records(1704067200, units, count=90)
    baseline, manager = open_pair(tmp_path, records)
    directives = {obs: 'software' for obs in ['pressure', 'altimeter', 'barometer', 'dewpoint', 'appTemp',
                                              'ET', 'windDir', 'rainRate', 'deltaRain']}
    try:
        monkeypatch.setitem(weewx.units.obs_group_dict, 'deltaRain', 'group_rain')
        original = Baseline.__new__(Baseline)
        original.db_manager = baseline
        service = StdWXCalculate.__new__(StdWXCalculate)
        service.db_manager = manager
        providers = [builtin_providers(), builtin_providers()]
        # Original scalars must use Python on the baseline, while the service
        # must avoid one scalar bridge call for each field on the Rust side.
        providers[0] = [p for p in providers[0] if type(p).__name__ != 'NativeXType']
        for i in [30, 31, 50, 70]:
            expected = copy.deepcopy(records[i])
            actual = copy.deepcopy(records[i])
            expected.pop('pressure'); actual.pop('pressure')
            if i == 50:
                expected.pop('outHumidity'); actual.pop('outHumidity')
            monkeypatch.setattr(weewx.xtypes, 'xtypes', providers[0])
            original.do_calculations(expected, directives)
            monkeypatch.setattr(weewx.xtypes, 'xtypes', providers[1])
            with forbid_python_weather():
                service.do_calculations(actual, directives)
            compare(actual, expected)
            pressure = next(p for p in providers[1] if type(p).__name__ == 'PressureCooker')
            reference = next(p for p in providers[0] if type(p).__name__ == 'PressureCooker')
            compare(pressure.temp_12h_vt, reference.temp_12h_vt)
    finally:
        baseline.close()
        manager.close()


@pytest.mark.parametrize('units', [1, 16, 17])
@pytest.mark.parametrize('observation', ['ET', 'pressure'])
def test_raw_fill_missing_normalizes_scalar_units(tmp_path, monkeypatch, units, observation):
    records = hardware_records(1704067200, units, count=40)
    for i, record in enumerate(records):
        record[observation] = None if i % 2 else 99.0
    baseline, manager = open_pair(tmp_path, records)
    span = weeutil.weeutil.TimeSpan(records[25]['dateTime'], records[-1]['dateTime'])
    try:
        providers = [p for p in builtin_providers() if type(p).__name__ != 'NativeXType']
        monkeypatch.setattr(weewx.xtypes, 'xtypes', providers)
        expected = []
        for record in records[26:]:
            value = weewx.xtypes.get_scalar(observation, record, baseline)
            value = weewx.units.convertStd(value, units)[0]
            expected.append(record[observation] if record[observation] is not None else value)
        monkeypatch.setattr(weewx.xtypes, 'xtypes', builtin_providers())
        actual = get_series_many([{'observation': observation, 'span': span,
                                  'options': {'source': 'fill_missing'}}], manager)[0]
        compare(actual[2][0], expected)
        assert actual[2][1:] == weewx.units.getStandardUnitType(units, observation)
    finally:
        baseline.close()
        manager.close()


def test_packet_custom_dependency_keeps_configuration_order(tmp_path, monkeypatch):
    from weewxdb.services import StdWXCalculate
    records = hardware_records(1704067200, 17, count=5)
    baseline, manager = open_pair(tmp_path, records)
    class Custom(weewx.xtypes.XType):
        def get_scalar(self, obs_type, record, db_manager=None, **options):
            if obs_type != 'outTemp':
                raise weewx.UnknownType(obs_type)
            return weewx.units.ValueTuple(35, 'degree_C', 'group_temperature')
    try:
        monkeypatch.setattr(weewx.xtypes, 'xtypes', [Custom()] + builtin_providers())
        service = StdWXCalculate.__new__(StdWXCalculate)
        service.db_manager = manager
        record = dict(records[0])
        service.do_calculations(record, {'outTemp': 'software', 'appTemp': 'software'})
        assert record['outTemp'] == 35
        expected = weewx.wxformulas.apptempC(35, record['outHumidity'], record['windSpeed'])
        assert record['appTemp'] == pytest.approx(expected)
    finally:
        baseline.close()
        manager.close()


def test_small_workspace_splits_sources_but_reuses_each_formula(tmp_path, monkeypatch):
    records = hardware_records(1704067200, 17, count=800)
    baseline, manager = open_pair(tmp_path, records)
    path = manager.connection.file_path
    manager.close()
    import weewx.manager
    manager = weewx.manager.Manager.open({'driver': 'weewxdb.driver', 'database_name': path,
                                        'workspace_mib': 1, 'cache_mib': 1})
    span = weeutil.weeutil.TimeSpan(records[0]['dateTime'] - 1800 + 1, records[-1]['dateTime'])
    requests = requests_for(['appTemp', 'windchill', 'dewpoint'], span, interval=1800)
    try:
        monkeypatch.setattr(weewx.xtypes, 'xtypes', builtin_providers())
        expected = [weewx.xtypes.get_series(r['observation'], span, baseline, r['aggregate'], r['interval']) for r in requests]
        statistics = {}
        actual = get_series_many(requests, manager, statistics=statistics)
        compare(actual, expected)
        assert statistics['scans'] == 3
        assert statistics['formula_evaluations'] == 3 * len(records)
    finally:
        baseline.close()
        manager.close()


def test_packet_falls_back_to_weewx_instead_of_stopping_the_engine(tmp_path, monkeypatch):
    from weewxdb.services import StdWXCalculate
    from weewxdb.almanac import NativeAlmanac
    records = hardware_records(1704067200, 17, count=5)
    baseline, manager = open_pair(tmp_path, records)

    class OtherAlmanac(weewx.almanac.AlmanacType):
        def get_almanac_data(self, almanac_obj, attr):
            raise weewx.UnknownType(attr)
    try:
        monkeypatch.setattr(weewx.xtypes, 'xtypes', builtin_providers())
        # An almanac extension the native maxSolarRad does not know.
        monkeypatch.setattr(weewx.almanac, 'almanacs', [OtherAlmanac(), NativeAlmanac(), weewx.almanac.WeeutilAlmanacType()])
        service = StdWXCalculate.__new__(StdWXCalculate)
        service.db_manager = manager
        record = dict(records[0], windGust=float('inf'))
        record.pop('maxSolarRad', None)
        service.do_calculations(record, {'maxSolarRad': 'prefer_hardware', 'dewpoint': 'software'})
        assert record['maxSolarRad'] is not None
        assert record['dewpoint'] == pytest.approx(weewx.wxformulas.dewpointC(record['outTemp'], record['outHumidity']))
        # A NaN input reaches WeeWX's formula, as without the service.
        record = dict(records[1], outTemp=float('nan'))
        service.do_calculations(record, {'dewpoint': 'software'})
        assert math.isnan(record['dewpoint'])
    finally:
        baseline.close()
        manager.close()
