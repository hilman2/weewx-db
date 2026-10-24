# Copyright (c) 2026 Manuel Hilgert <hilman2@gmail.com>
"""weectl database actions leave the Rust database like its SQLite twin."""

import time

import configobj
import pytest

import weewx
import weewx.manager
import weewx.xtypes
import weectllib.database_actions as actions

SCHEMA = 'weewx.schemas.wview_small.schema'


@pytest.fixture
def twins(tmp_path, monkeypatch):
    monkeypatch.setenv('TZ', 'UTC')
    time.tzset()
    # calc-missing loads XType services, which register providers globally.
    monkeypatch.setattr(weewx.xtypes, 'xtypes', list(weewx.xtypes.xtypes))
    configs = []
    for name, database in [('sqlite', {'database_type': 'SQLite', 'database_name': 'twin.sdb'}),
                           ('rust', {'database_type': 'SQLite', 'driver': 'weewxdb.driver',
                                     'database_name': 'twin.wwdb', 'sync_mode': 'background'})]:
        configs.append(configobj.ConfigObj({
            'WEEWX_ROOT': str(tmp_path),
            'Station': {'altitude': ['100', 'meter'], 'latitude': '52', 'longitude': '13'},
            'StdWXCalculate': {'Calculations': {'dewpoint': 'software', 'windchill': 'software'}},
            'Engine': {'Services': dict({group: [] for group in weewx.all_service_groups},
                                        xtype_services=['weewx.wxxtypes.StdWXXTypes'])},
            'DataBindings': {'wx_binding': {'database': name, 'table_name': 'archive', 'schema': SCHEMA,
                                            'manager': 'weewx.manager.DaySummaryManager'}},
            'Databases': {name: database},
            'DatabaseTypes': {'SQLite': {'driver': 'weedb.sqlite', 'SQLITE_ROOT': str(tmp_path)}}}))
    start = 1700006400
    # 288 records a day: more than one fetch batch of the streaming cursor.
    records = [{'dateTime': start + 300 * i, 'usUnits': weewx.METRIC, 'interval': 5,
                'outTemp': float(i % 25), 'outHumidity': 60.0, 'windSpeed': float(i % 5),
                'windDir': 90.0, 'rain': 0.2 if i % 7 == 0 else 0.0,
                # calc-missing fills these in
                'dewpoint': None if i % 3 else 1.0} for i in range(1, 3 * 288 + 1)]
    for config in configs:
        with weewx.manager.open_manager_with_config(config, 'wx_binding', initialize=True) as manager:
            manager.addRecord(records)
    yield configs
    monkeypatch.undo()
    time.tzset()


def contents(config):
    with weewx.manager.open_manager_with_config(config, 'wx_binding') as manager:
        result = {}
        for table in sorted(manager.connection.tables()):
            columns = manager.connection.columnsOf(table)
            rows = list(manager.genSql('SELECT * FROM %s ORDER BY 1' % table))
            result[table] = (columns, rows)
        return result


def same(a, b):
    assert a.keys() == b.keys()
    for table in a:
        assert a[table][0] == b[table][0], table
        assert len(a[table][1]) == len(b[table][1]), table
        for left, right in zip(a[table][1], b[table][1]):
            assert left == pytest.approx(right, rel=1e-12, abs=1e-9), (table, left, right)


def test_database_actions_match_sqlite(twins):
    for config in twins:
        actions.drop_daily(config, no_confirm=True)
        actions.rebuild_daily(config, no_confirm=True)
        actions.add_column(config, 'pm25', 'REAL', no_confirm=True)
        actions.rename_column(config, 'pm25', 'pm2_5', no_confirm=True)
        actions.drop_columns(config, ['pm2_5', 'windchill'], no_confirm=True)
        actions.calc_missing(config, no_confirm=True)
        actions.reweight_daily(config, no_confirm=True)
        actions.update_database(config, no_confirm=True)
        actions.check(config)
    sqlite, rust = (contents(config) for config in twins)
    assert 'archive_day_pm2_5' not in rust and 'windchill' not in rust['archive'][0]
    dewpoint = rust['archive'][0].index('dewpoint')
    assert all(row[dewpoint] is not None for row in rust['archive'][1])
    same(rust, sqlite)
