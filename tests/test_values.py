# Copyright (c) 2026 Manuel Hilgert <hilman2@gmail.com>
"""Values SQLite stores reach the Rust database unchanged, as WeeWX writes them."""

import time

import pytest

import weeutil.weeutil
import weewx
import weewx.accum
import weewx.manager

SCHEMA = {'table': [('dateTime', 'INTEGER NOT NULL UNIQUE PRIMARY KEY'), ('usUnits', 'INTEGER NOT NULL'),
                    ('interval', 'INTEGER NOT NULL'), ('outTemp', 'REAL'), ('outHumidity', 'REAL')],
          'day_summaries': [('outTemp', 'scalar'), ('outHumidity', 'scalar')]}


@pytest.fixture
def twins(tmp_path, monkeypatch):
    monkeypatch.setenv('TZ', 'UTC')
    time.tzset()
    weewx.accum.initialize({})
    managers = [weewx.manager.DaySummaryManager.open_with_create(binding, schema=SCHEMA) for binding in [
        {'driver': 'weedb.sqlite', 'database_name': str(tmp_path / 'twin.sdb')},
        {'driver': 'weewxdb.driver', 'database_name': str(tmp_path / 'twin.wwdb'), 'sync_mode': 'background'}]]
    yield managers
    for manager in managers:
        manager.close()
    monkeypatch.undo()
    time.tzset()


def day_row(manager, table):
    return manager.getSql('SELECT min, mintime, max, maxtime, sum, count, sumtime FROM %s' % table)


def test_fractional_loop_times_keep_daily_extremes(twins):
    # A driver with float packet times makes mintime and maxtime REAL values
    # in INTEGER columns. The day must keep every record and its extremes.
    start = 1700006400
    for manager in twins:
        for i in range(1, 49):
            stop = start + 300 * i
            accumulator = weewx.accum.Accum(weeutil.weeutil.TimeSpan(stop - 300, stop))
            for k in range(10):
                accumulator.addRecord({'dateTime': stop - 300 + 30 * k + 0.37, 'usUnits': weewx.METRIC,
                                       'outTemp': float((i * 10 + k) % 23), 'outHumidity': 50.0})
            manager.addRecord({'dateTime': stop, 'usUnits': weewx.METRIC, 'interval': 5,
                               'outTemp': float(i % 23), 'outHumidity': 50.0}, accumulator=accumulator)
    assert day_row(twins[1], 'archive_day_outTemp') == day_row(twins[0], 'archive_day_outTemp')
    assert day_row(twins[1], 'archive_day_outTemp')[5] == 48


def test_infinity_and_text_like_sqlite(twins):
    records = [{'dateTime': 1700006400 + 300 * i, 'usUnits': weewx.METRIC, 'interval': 5,
                'outTemp': 1.0, 'outHumidity': 50.0} for i in range(1, 4)]
    records[1]['outTemp'] = float('inf')
    for manager in twins:
        manager.addRecord(records, log_failure=False)
    assert twins[1].getSql('SELECT MAX(outTemp), COUNT(*) FROM archive') == (float('inf'), 3)
    # SQLite stores the text; this database rejects it, and WeeWX skips
    # only that record.
    bad = dict(records[2], dateTime=records[2]['dateTime'] + 300, outTemp='N/A')
    good = dict(bad, dateTime=bad['dateTime'] + 300, outTemp=2.0)
    assert twins[1].addRecord([bad, good], log_failure=False) == 1
    assert twins[1].getSql('SELECT COUNT(*) FROM archive') == (4,)
