"""Connections of one process share parsed indices and replayed journals."""

import pytest

weewxdb = pytest.importorskip('weewxdb')


def rows(path):
    connection = weewxdb.connect(path)
    try:
        return connection.execute('SELECT dateTime, value FROM archive ORDER BY dateTime').fetchall()
    finally:
        connection.close()


def test_new_connections_see_every_published_frame_and_nothing_else(tmp_path):
    path = tmp_path / 'shared.wwdb'
    weewxdb._weewxdb.create(str(path))
    writer = weewxdb.connect(path)
    writer.execute('CREATE TABLE archive(dateTime INTEGER PRIMARY KEY, value REAL)').close()
    writer.native.checkpoint()
    writer.execute('INSERT INTO archive VALUES (300, 1.0)').close()
    assert rows(path) == [(300, 1.0)]
    # The next connection continues from the replayed state.
    writer.execute('INSERT INTO archive VALUES (600, 2.0)').close()
    writer.execute('UPDATE archive SET value = 1.5 WHERE dateTime = 300').close()
    assert rows(path) == [(300, 1.5), (600, 2.0)]
    # Rolling back reloads the writer from the shared state, which must not
    # contain the rolled-back rows.
    writer.execute('BEGIN').close()
    writer.execute('INSERT INTO archive VALUES (900, 3.0)').close()
    writer.execute('DELETE FROM archive WHERE dateTime = 300').close()
    writer.execute('ROLLBACK').close()
    assert writer.execute('SELECT COUNT(*) FROM archive').fetchone() == (2,)
    assert rows(path) == [(300, 1.5), (600, 2.0)]
    writer.execute('ALTER TABLE archive ADD COLUMN extra REAL').close()
    writer.execute('INSERT INTO archive VALUES (1200, 4.0, 9.0)').close()
    assert rows(path) == [(300, 1.5), (600, 2.0), (1200, 4.0)]
    writer.native.checkpoint()
    # The new journal grows past the length the old journal had been read
    # to, so only its identity tells the two apart.
    later = [(1500 + 300 * i, float(i)) for i in range(40)]
    for time, value in later:
        writer.execute('INSERT INTO archive VALUES (?, ?, NULL)', (time, value)).close()
    assert rows(path) == [(300, 1.5), (600, 2.0), (1200, 4.0)] + later
    writer.close()
