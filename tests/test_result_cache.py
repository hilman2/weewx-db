"""Cached aggregate results must follow every committed change."""

import pytest

weewxdb = pytest.importorskip('weewxdb')

import weewx.manager
import weewx.schemas.wview_small
import weewx.xtypes


def record(time, temperature):
    return {'dateTime': time, 'usUnits': 1, 'interval': 5, 'outTemp': temperature}


def test_aggregate_cache_sees_other_connection_own_write_and_transaction(tmp_path):
    binding = {'driver': 'weewxdb.driver', 'database_name': str(tmp_path / 'cache.wwdb')}
    start = 1704067200
    span = (start, start + 3600)
    manager = weewx.manager.DaySummaryManager.open_with_create(binding, schema=weewx.schemas.wview_small.schema)
    manager.addRecord(record(start + 300, 50.0))
    maximum = lambda: weewx.xtypes.get_aggregate('outTemp', span, 'max', manager)[0]
    assert maximum() == 50.0
    assert maximum() == 50.0
    other = weewx.manager.DaySummaryManager.open(binding)
    other.addRecord(record(start + 600, 60.0))
    other.close()
    assert maximum() == 60.0
    manager.addRecord(record(start + 900, 70.0))
    assert maximum() == 70.0
    # Uncommitted writes change results without a new revision.
    manager.connection.begin()
    assert maximum() == 70.0
    manager.connection.execute('UPDATE archive SET outTemp = 80.0 WHERE dateTime = ?', (start + 900,))
    assert maximum() == 80.0
    manager.connection.rollback()
    assert maximum() == 70.0
    manager.close()


def test_engine_aggregate_memo_follows_writes_rollback_and_checkpoint(tmp_path):
    path = tmp_path / 'memo.wwdb'
    weewxdb._weewxdb.create(str(path))
    connection = weewxdb.connect(path)
    connection.execute('CREATE TABLE archive(dateTime INTEGER PRIMARY KEY, value REAL)').close()
    connection.execute('INSERT INTO archive VALUES (300, 1.0)').close()
    newest = lambda: connection.execute('SELECT MAX(dateTime), MAX(value) FROM archive').fetchone()
    assert newest() == (300, 1.0)
    with connection:
        connection.execute('INSERT INTO archive VALUES (600, 2.0)').close()
        assert newest() == (600, 2.0)
        connection.execute('ROLLBACK').close()
    assert newest() == (300, 1.0)
    other = weewxdb.connect(path)
    other.execute('INSERT INTO archive VALUES (900, 3.0)').close()
    other.close()
    assert newest() == (900, 3.0)
    connection.native.checkpoint()
    connection.execute('DELETE FROM archive WHERE dateTime = 900').close()
    assert newest() == (300, 1.0)
    connection.close()
