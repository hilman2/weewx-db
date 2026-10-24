# Copyright (c) 2026 Manuel Hilgert <hilman2@gmail.com>
"""Months of weewxd operation keep blocks full and the file bounded.

`just full` sets WEEWXDB_LONG_TESTS=1 for the WeeWX schema and a longer run.
"""

import json
import math
import os
import pathlib
import subprocess
import time

import pytest

import weewx
import weewx.manager
import weewx.schemas.wview_extended

ROOT = pathlib.Path(__file__).resolve().parents[1]
LONG = os.environ.get('WEEWXDB_LONG_TESTS') == '1'
SMALL = {'table': [('dateTime', 'INTEGER NOT NULL UNIQUE PRIMARY KEY'), ('usUnits', 'INTEGER NOT NULL'),
                   ('interval', 'INTEGER NOT NULL'), ('outTemp', 'REAL'), ('rain', 'REAL'), ('windSpeed', 'REAL')],
         'day_summaries': [('outTemp', 'scalar'), ('rain', 'scalar'), ('windSpeed', 'scalar')]}


@pytest.fixture
def utc(monkeypatch):
    monkeypatch.setenv('TZ', 'UTC')
    time.tzset()
    yield
    monkeypatch.undo()
    time.tzset()


def info(path):
    result = subprocess.run([str(ROOT / 'target/release/weewx-db'), 'info', str(path)],
                            capture_output=True, text=True, check=True)
    return json.loads(result.stdout)


def test_one_record_per_transaction_for_weeks(tmp_path, utc):
    schema = weewx.schemas.wview_extended.schema if LONG else SMALL
    days = 60 if LONG else 12
    path = tmp_path / 'station.wwdb'
    binding = {'driver': 'weewxdb.driver', 'database_name': str(path), 'sync_mode': 'background'}
    start = 1700006400
    live = []
    with weewx.manager.DaySummaryManager.open_with_create(binding, schema=schema) as manager:
        for i in range(1, days * 288 + 1):
            manager.addRecord({'dateTime': start + 300 * i, 'usUnits': weewx.METRIC, 'interval': 5,
                               'outTemp': float(i % 40), 'rain': 0.1 if i % 9 == 0 else 0.0,
                               'windSpeed': float(i % 7)}, log_success=False)
            if i % 864 == 0:
                state = info(path)
                live.append(state['file_bytes'] - state['free_bytes'])
    state = info(path)
    assert state['rows'] == days * 288
    # Compacted rows fill blocks to their capacity: 4096 archive rows or 128
    # daily rows. Rows waiting for the next compaction are in no block yet.
    for table, (rows, blocks) in state['blocks'].items():
        assert blocks == math.ceil(rows / (128 if '_day_' in table else 4096)), (table, rows, blocks)
    assert state['blocks']['archive'][0] + state['changed_rows'] >= days * 288
    # Pages of replaced blocks are reused: the file grows with the data, not
    # with the number of compactions.
    assert state['file_bytes'] < 3 * live[-1] + 4 * 1024 * 1024, (state, live)
    result = subprocess.run([str(ROOT / 'target/release/weewx-db'), 'check', str(path)], capture_output=True, text=True)
    assert result.stdout.strip() == 'ok', result.stderr
