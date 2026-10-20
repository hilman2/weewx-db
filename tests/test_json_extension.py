"""Exercise the installed WeeWX generator rather than a bundled copy."""

import json
import sys
from pathlib import Path

import pytest

base = pytest.importorskip('weewx.jsongenerator')
import configobj
import weeutil.weeutil
import weewx.units
import weewx.xtypes
from weewxdb.json import JSONGenerator, daynight
from test_all_xtypes import builtin_providers, compare, hardware_records, open_pair

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'tools'))
from bench_json import generator, outputs


@pytest.mark.parametrize('extremes', [[], ['appTemp', 'windchill']])
def test_full_rebuild_incremental_and_new_line_match_base(tmp_path, monkeypatch, extremes):
    records = hardware_records(1704067200, 17, count=150)
    baseline, manager = open_pair(tmp_path, records, daily=True)
    baseline.close(); manager.close()
    roots = [tmp_path / 'base', tmp_path / 'batch']
    instances = [generator({'driver': driver, 'database_name': str(tmp_path / filename)}, root,
                           records[-2]['dateTime'], ['appTemp', 'windchill'])
                 for driver, filename, root in [('weedb.sqlite', 'source.sdb', roots[0]),
                                                 ('weewxdb.driver', 'live.wwdb', roots[1])]]
    old = instances[1]
    instances[1] = JSONGenerator(old.config_dict, old.skin_dict, old.gen_ts, True, old.stn_info)
    old.finalize()
    for instance in instances:
        instance.skin_dict['JSONGenerator']['Archive']['extremes'] = extremes
        instance.skin_dict['JSONGenerator']['JSONFiles'] = {'temperature': {'appTemp': {}, 'windchill': {}}}
    try:
        for phase in ['rebuild', 'incremental', 'new_line', 'idle']:
            for instance in instances:
                if phase != 'rebuild':
                    instance.gen_ts = records[-1]['dateTime']
                    instance.first_run = False
                if phase == 'new_line':
                    instance.skin_dict['JSONGenerator']['JSONFiles']['temperature']['dewpoint'] = {}
                monkeypatch.setattr(weewx.xtypes, 'xtypes', builtin_providers())
                instance.start()
            expected, actual = [outputs(root) for root in roots]
            assert expected
            compare(actual, expected)
    finally:
        for instance in instances:
            instance.finalize()


def test_derived_all_null_column_is_not_skipped(tmp_path, monkeypatch):
    records = hardware_records(1704067200, 17, count=100)
    for record in records:
        record['appTemp'] = None
    baseline, manager = open_pair(tmp_path, records, daily=True)
    baseline.close(); manager.close()
    instance = generator({'driver': 'weewxdb.driver', 'database_name': str(tmp_path / 'live.wwdb')},
                         tmp_path / 'output', records[-1]['dateTime'], ['appTemp'])
    adapted = JSONGenerator(instance.config_dict, instance.skin_dict, instance.gen_ts, True, instance.stn_info)
    instance.finalize()
    adapted.skin_dict['JSONGenerator']['JSONFiles']['appTemp']['appTemp']['source'] = 'derived'
    try:
        monkeypatch.setattr(weewx.xtypes, 'xtypes', builtin_providers())
        adapted.start()
        files = [v for k,v in outputs(tmp_path / 'output').items() if 'series' in v]
        assert files
        assert any(any(value is not None for value in series['values'])
                   for file in files for series in file['series'])
    finally:
        adapted.finalize()


def test_daynight_uses_native_astronomy_without_weeutil_sun(monkeypatch):
    import weeutil.Sun
    monkeypatch.setattr(weeutil.Sun, 'sunRiseSet', lambda *a: pytest.fail('Python solar calculations used'))
    start = 1718928000
    result = daynight(start, start + 2 * 86400, 52, 13)
    assert len(result['transitions']) == 4
    assert len(result['twilight']) == 4
    assert result['first'] == 'night'
    import ephem
    observer = ephem.Observer(); observer.lat = '52'; observer.lon = '13'
    observer.date = ephem.Date(start / 86400 + 25567.5)
    sun = ephem.Sun()
    first = (float(observer.next_rising(sun)) - 25567.5) * 86400
    assert result['transitions'][0] == pytest.approx(first, abs=1)
    assert daynight(start, start + 86400, 89, 13) is None


def test_incremental_resume_calendar_is_reused(tmp_path, monkeypatch):
    import collections
    records = hardware_records(1704067200, 17, count=150)
    baseline, manager = open_pair(tmp_path, records, daily=True)
    baseline.close(); manager.close()
    instance = generator({'driver': 'weewxdb.driver', 'database_name': str(tmp_path / 'live.wwdb')},
                         tmp_path / 'output', records[-2]['dateTime'], ['appTemp', 'windchill'], JSONGenerator)
    instance.skin_dict['JSONGenerator']['Archive']['extremes'] = ['appTemp', 'windchill']
    try:
        monkeypatch.setattr(weewx.xtypes, 'xtypes', builtin_providers())
        instance.start()
        original = base._intervals
        calls = collections.Counter()
        def intervals(start, stop, seconds):
            calls[start, stop, seconds] += 1
            return original(start, stop, seconds)
        monkeypatch.setattr(base, '_intervals', intervals)
        instance.gen_ts = records[-1]['dateTime']; instance.first_run = False
        instance.start()
        assert calls
        assert max(calls.values()) == 1
    finally:
        instance.finalize()


def test_changed_generator_interface_delegates_to_installed_base(tmp_path, monkeypatch):
    def future_line(self, *args, **kwargs):
        return 'future generator'
    monkeypatch.setattr(base.JSONGenerator, '_aggregate_line', future_line)
    instance = generator({'driver': 'weedb.sqlite', 'database_name': str(tmp_path / 'unused.sdb')},
                         tmp_path, 1704067200, generator_class=JSONGenerator)
    try:
        assert not instance._supported
        assert instance._aggregate_line() == 'future generator'
    finally:
        instance.finalize()
