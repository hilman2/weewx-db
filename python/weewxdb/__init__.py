"""DB-API access to the Rust WeeWX database."""

import weakref
import re

from . import _weewxdb

apilevel = '2.0'
threadsafety = 1
paramstyle = 'qmark'


def astronomy(timestamp, latitude, longitude, elevation=0.0, body='sun',
              pressure=1010.0, temperature=15.0, horizon=0.0,
              operation='position', start=None, stop=None, center=False):
    """Calculate a position, event range or lunar phases in the Rust core."""
    return _weewxdb.astronomy_native({
        'time': timestamp, 'latitude': latitude, 'longitude': longitude, 'elevation': elevation,
        'body': body, 'pressure': pressure, 'temperature': temperature, 'horizon': horizon,
        'operation': operation, 'start': start, 'stop': stop, 'center': center})


class Error(Exception):
    pass


class DatabaseError(Error):
    pass


class IntegrityError(DatabaseError):
    pass


class OperationalError(DatabaseError):
    pass


class ProgrammingError(DatabaseError):
    pass


class NotSupportedError(DatabaseError):
    pass


def _call(function, *args, **kwargs):
    try:
        return function(*args, **kwargs)
    except ValueError as error:
        message = str(error)
        if 'constraint failed' in message:
            raise IntegrityError(message) from error
        if any(text in message for text in [
                'closed', 'number of SQL bindings', 'transaction already active',
                'one SQL statement', 'execute accepts one']):
            raise ProgrammingError(message) from error
        raise OperationalError(message) from error


def connect(database, timeout=5.0, cache_mib=32, autocommit=True, workspace_mib=32, sync_mode='background'):
    return Connection(database, timeout=timeout, cache_mib=cache_mib,
                      autocommit=autocommit, workspace_mib=workspace_mib, sync_mode=sync_mode)


class Connection:
    def __init__(self, database, timeout=5.0, cache_mib=32, autocommit=True, workspace_mib=32, sync_mode='background'):
        self.native = _call(_weewxdb.Connection, str(database), cache_mib, timeout, workspace_mib, sync_mode)
        self.autocommit = autocommit
        self._in_transaction = False
        self._closed = False
        self._cursors = weakref.WeakSet()

    def cursor(self):
        if self._closed:
            raise ProgrammingError('Connection is closed')
        cursor = Cursor(self)
        self._cursors.add(cursor)
        return cursor

    def execute(self, sql, parameters=()):
        return self.cursor().execute(sql, parameters)

    def executemany(self, sql, parameters):
        return self.cursor().executemany(sql, parameters)

    def commit(self):
        if self._in_transaction:
            self.execute('COMMIT').close()

    def rollback(self):
        if self._in_transaction:
            self.execute('ROLLBACK').close()

    def flush(self):
        _call(self.native.flush)

    def close(self):
        if not self._closed:
            for cursor in list(self._cursors):
                cursor.close()
            _call(self.native.close)
            self._closed = True

    def __enter__(self):
        self.execute('BEGIN').close()
        return self

    def __exit__(self, exception_type, exception, traceback):
        if exception_type is None:
            self.commit()
        else:
            self.rollback()


class Cursor:
    def __init__(self, connection):
        self.connection = connection
        self.arraysize = 200
        self.description = None
        self.rowcount = -1
        self.lastrowid = None
        self._native = None
        self._closed = False

    def execute(self, sql, parameters=()):
        if self._closed or self.connection._closed:
            raise ProgrammingError('Cursor or connection is closed')
        if self._native is not None:
            _call(self._native.close)
        words = re.sub(r'\A(?:\s+|--[^\n]*(?:\n|$)|/\*.*?\*/)*', '',
                       sql, flags=re.DOTALL).split(None, 1)
        if not words:
            raise ProgrammingError('SQL statement is empty')
        keyword = words[0].upper()
        if not self.connection.autocommit and not self.connection._in_transaction \
                and keyword in ['INSERT', 'UPDATE', 'DELETE', 'REPLACE']:
            self.connection.execute('BEGIN').close()
        self._native = _call(self.connection.native.execute, sql, tuple(parameters))
        self.description = [(name, None, None, None, None, None, None)
                            for name in self._native.columns] or None
        self.rowcount = self._native.rowcount
        self.connection._in_transaction = self.connection.native.in_transaction
        return self

    def executemany(self, sql, parameters):
        own_transaction = not self.connection._in_transaction
        if own_transaction:
            self.connection.execute('BEGIN').close()
        count = 0
        try:
            for values in parameters:
                self.execute(sql, values)
                count += max(self.rowcount, 0)
            if own_transaction:
                self.connection.commit()
        except Exception:
            if own_transaction:
                self.connection.rollback()
            raise
        self.rowcount = count
        return self

    def fetchone(self):
        self._check_result()
        return _call(self._native.fetchone)

    def fetchmany(self, size=None):
        self._check_result()
        return _call(self._native.fetchmany, self.arraysize if size is None else size)

    def fetchall(self):
        result = []
        while True:
            rows = self.fetchmany()
            if not rows:
                return result
            result.extend(rows)

    def _check_result(self):
        if self._closed or self.connection._closed or self._native is None:
            raise ProgrammingError('Cursor has no active result')

    def close(self):
        if not self._closed:
            if self._native is not None:
                _call(self._native.close)
            self._closed = True

    def __iter__(self):
        return self

    def __next__(self):
        row = self.fetchone()
        if row is None:
            raise StopIteration
        return row

    def __enter__(self):
        return self

    def __exit__(self, exception_type, exception, traceback):
        self.close()

    def setinputsizes(self, sizes):
        pass

    def setoutputsize(self, size, column=None):
        pass
