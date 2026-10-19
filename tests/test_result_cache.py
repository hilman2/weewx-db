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


def test_shared_cache_survives_unrelated_writes_and_sees_related_ones(tmp_path, monkeypatch):
    import subprocess
    import sys
    from weewxdb import native
    binding = {'driver': 'weewxdb.driver', 'database_name': str(tmp_path / 'shared.wwdb')}
    start = 1704067200
    span = (start, start + 3600)
    writer = weewx.manager.DaySummaryManager.open_with_create(binding, schema=weewx.schemas.wview_small.schema)
    writer.addRecord(record(start + 300, 50.0))
    # Results are shared across connections for spans that end well before
    # the newest record; later spans move with the report time.
    writer.addRecord(record(start + 5 * 86400, 10.0))
    requests = []
    original = native.request
    monkeypatch.setattr(native, 'request', lambda manager, query: requests.append(query) or original(manager, query))

    def maximum():
        # Every report run opens its own manager.
        with weewx.manager.DaySummaryManager.open(binding) as manager:
            return weewx.xtypes.get_aggregate('outTemp', span, 'max', manager)[0]

    assert maximum() == 50.0
    writer.addRecord(record(start + 5 * 86400 + 300, 99.0))
    assert maximum() == 50.0
    assert len(requests) == 1, 'a write outside the span must keep the result'
    writer.connection.execute('UPDATE archive SET outTemp = 55.0 WHERE dateTime = ?', (start + 300,))
    assert maximum() == 55.0
    writer.addRecord(record(start + 600, 60.0))
    assert maximum() == 60.0
    writer.connection.native.checkpoint()
    assert maximum() == 60.0
    # Another process writes and folds its frame into a checkpoint, so this
    # process never sees the frame itself.
    code = ('import sys, weewxdb\n'
            'c = weewxdb.connect(sys.argv[1])\n'
            'c.execute("UPDATE archive SET outTemp = 65.0 WHERE dateTime = ?", (%d,)).close()\n'
            'c.native.checkpoint(); c.close()\n' % (start + 600))
    subprocess.run([sys.executable, '-c', code, binding['database_name']], check=True)
    assert maximum() == 65.0
    writer.close()


def test_recent_spans_stay_with_their_connection_and_revision(tmp_path, monkeypatch):
    from weewxdb import native
    binding = {'driver': 'weewxdb.driver', 'database_name': str(tmp_path / 'recent.wwdb')}
    start = 1704067200
    span = (start, start + 3600)
    writer = weewx.manager.DaySummaryManager.open_with_create(binding, schema=weewx.schemas.wview_small.schema)
    writer.addRecord(record(start + 300, 50.0))
    requests = []
    original = native.request
    monkeypatch.setattr(native, 'request', lambda manager, query: requests.append(query) or original(manager, query))
    with weewx.manager.DaySummaryManager.open(binding) as manager:
        maximum = lambda: weewx.xtypes.get_aggregate('outTemp', span, 'max', manager)[0]
        assert maximum() == maximum() == 50.0
        assert len(requests) == 1
        writer.addRecord(record(start + 600, 60.0))
        assert maximum() == 60.0
    writer.close()
