"""ORDER BY and LIMIT on the virtual tables must return SQLite's rows."""

import random
import sqlite3

import pytest

weewxdb = pytest.importorskip('weewxdb')

SCHEMA = 'CREATE TABLE archive_day_rain(dateTime INTEGER PRIMARY KEY, sum REAL, count INTEGER, max REAL)'


def rows(seed, count):
    generator = random.Random(seed)
    for index in range(count):
        # Ties, NULL values and integer-valued reals exercise SQLite's order.
        total = None if index % 17 == 0 else float(generator.randrange(0, 40)) / 4
        yield (86400 * index, total, generator.randrange(0, 4), None if index % 29 == 0 else generator.uniform(-5, 9))


@pytest.fixture(scope='module')
def engines(tmp_path_factory):
    path = tmp_path_factory.mktemp('ordering') / 'ordered.wwdb'
    weewxdb._weewxdb.create(str(path))
    native = weewxdb.connect(path)
    reference = sqlite3.connect(':memory:')
    for connection in [native, reference]:
        connection.execute(SCHEMA).close()
    data = list(rows(1, 1500))
    native.executemany('INSERT INTO archive_day_rain VALUES (?,?,?,?)', data).close()
    reference.executemany('INSERT INTO archive_day_rain VALUES (?,?,?,?)', data)
    native.native.checkpoint()
    # Journal rows replace, delete and append after the checkpoint.
    changes = [('UPDATE archive_day_rain SET sum = 99.0 WHERE dateTime = ?', (86400 * 700,)),
               ('UPDATE archive_day_rain SET sum = NULL, count = 3 WHERE dateTime = ?', (86400 * 12,)),
               ('DELETE FROM archive_day_rain WHERE dateTime = ?', (86400 * 31,)),
               ('INSERT INTO archive_day_rain VALUES (?,?,?,?)', (86400 * 1600, 9.75, 2, 1.0)),
               ('INSERT INTO archive_day_rain VALUES (?,?,?,?)', (-86400, 9.75, 0, None))]
    for sql, parameters in changes:
        native.execute(sql, parameters).close()
        reference.execute(sql, parameters)
    yield native, reference
    native.close()


QUERIES = [
    'SELECT dateTime, sum FROM archive_day_rain ORDER BY sum DESC LIMIT 1',
    'SELECT dateTime, sum FROM archive_day_rain ORDER BY sum DESC, dateTime LIMIT 7',
    'SELECT dateTime, sum FROM archive_day_rain ORDER BY sum ASC, dateTime ASC LIMIT 40',
    'SELECT dateTime, sum FROM archive_day_rain ORDER BY sum DESC, dateTime LIMIT 5 OFFSET 3',
    'SELECT dateTime, sum FROM archive_day_rain WHERE count > 0 ORDER BY sum DESC, dateTime LIMIT 3',
    'SELECT dateTime, sum FROM archive_day_rain WHERE dateTime >= 86400000 ORDER BY sum DESC, dateTime LIMIT 2',
    'SELECT dateTime, max FROM archive_day_rain WHERE max IS NOT NULL ORDER BY max ASC, dateTime LIMIT 4',
    'SELECT dateTime, max FROM archive_day_rain WHERE max < 0.5 AND count >= 2 ORDER BY max DESC, dateTime LIMIT 6',
    'SELECT dateTime, count FROM archive_day_rain WHERE sum = 2.5 ORDER BY count DESC, dateTime LIMIT 10',
    'SELECT dateTime, sum FROM archive_day_rain WHERE sum > \'2.5\' ORDER BY sum DESC, dateTime LIMIT 3',
    'SELECT dateTime, sum FROM archive_day_rain ORDER BY sum DESC, dateTime LIMIT -1',
    'SELECT MAX(dateTime) FROM archive_day_rain WHERE sum > 9',
    'SELECT MIN(dateTime) FROM archive_day_rain WHERE sum > 9',
    'SELECT MAX(dateTime) FROM archive_day_rain WHERE count = 3 AND dateTime < 86400000',
    'SELECT dateTime, sum FROM archive_day_rain WHERE dateTime < 86400000 ORDER BY dateTime DESC LIMIT 5',
    'SELECT dateTime FROM archive_day_rain WHERE dateTime > 86400 * 1400 ORDER BY dateTime DESC',
    'SELECT dateTime FROM archive_day_rain WHERE dateTime IN (86400, 172800, 864000) ORDER BY dateTime DESC',
    'SELECT dateTime, count FROM archive_day_rain WHERE count IN (1, 3) AND dateTime < 8640000 ORDER BY dateTime DESC',
    'SELECT dateTime, sum FROM archive_day_rain WHERE count IN (0, 3) ORDER BY sum DESC, dateTime LIMIT 5',
    'SELECT dateTime, sum FROM archive_day_rain WHERE sum != 9.75 ORDER BY sum DESC, dateTime LIMIT 3',
    'SELECT dateTime, sum FROM archive_day_rain WHERE sum IS NOT 9.75 ORDER BY sum DESC, dateTime LIMIT 3',
    'SELECT dateTime, sum FROM archive_day_rain WHERE sum IS NOT NULL ORDER BY sum ASC, dateTime LIMIT 3',
    'SELECT dateTime, max FROM archive_day_rain WHERE sum IS NULL ORDER BY max DESC, dateTime LIMIT 4',
    'SELECT dateTime, sum FROM archive_day_rain WHERE sum IS NULL ORDER BY sum DESC, dateTime LIMIT 2',
    'SELECT dateTime, max FROM archive_day_rain WHERE max IS 9.75 OR max IS NULL ORDER BY max DESC, dateTime LIMIT 2',
    'SELECT dateTime, sum FROM archive_day_rain WHERE sum IS NOT 9.75 AND sum != 99 ORDER BY sum DESC, dateTime LIMIT 2',
    'SELECT dateTime, sum FROM archive_day_rain WHERE sum IS NULL AND dateTime < 864000 ORDER BY dateTime DESC',
]


@pytest.mark.parametrize('sql', QUERIES)
def test_order_and_limit_match_sqlite(engines, sql):
    native, reference = engines
    assert native.execute(sql).fetchall() == [tuple(row) for row in reference.execute(sql).fetchall()]


@pytest.mark.parametrize('operator', ['IS', 'IS NOT'])
@pytest.mark.parametrize('value', [None, 9.75])
def test_is_with_bound_value_matches_sqlite(engines, operator, value):
    native, reference = engines
    sql = 'SELECT dateTime, sum FROM archive_day_rain WHERE sum %s ? ORDER BY sum DESC, dateTime LIMIT 4' % operator
    assert native.execute(sql, (value,)).fetchall() == [tuple(row) for row in reference.execute(sql, (value,)).fetchall()]


EXPRESSIONS = [
    ('SELECT dateTime, sum / count FROM archive_day_rain WHERE count > 0 ORDER BY sum / count DESC, dateTime ASC LIMIT 2', ()),
    ('SELECT dateTime, max - sum FROM archive_day_rain WHERE max IS NOT NULL ORDER BY max - sum DESC, dateTime LIMIT 3', ()),
    ('SELECT dateTime, count / 2 FROM archive_day_rain ORDER BY count / 2 DESC, dateTime LIMIT 5', ()),
    ('SELECT dateTime, sum / (count - 1) FROM archive_day_rain ORDER BY sum / (count - 1) DESC, dateTime LIMIT 4', ()),
    ('SELECT dateTime, (max - sum) AS spread FROM archive_day_rain WHERE dateTime >= 8640000 AND dateTime < 34560000 '
     'ORDER BY spread DESC LIMIT 3', ()),
    ('SELECT dateTime, -max * 2 FROM archive_day_rain ORDER BY -max * 2 ASC, dateTime DESC LIMIT 3 OFFSET 2', ()),
    ('SELECT dateTime, max - sum FROM archive_day_rain WHERE count >= ? ORDER BY max - sum ASC, dateTime LIMIT ?', (2, 3)),
    ('SELECT * FROM archive_day_rain WHERE sum != 9.75 ORDER BY sum + max DESC LIMIT 2', ()),
]


@pytest.mark.parametrize('sql,parameters', EXPRESSIONS)
def test_order_by_expression_matches_sqlite(engines, sql, parameters):
    native, reference = engines
    assert native.execute(sql, parameters).fetchall() == [tuple(row) for row in reference.execute(sql, parameters).fetchall()]
