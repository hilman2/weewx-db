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
        'operation': operation, 'start': start, 'stop': stop, 'center': bool(center)})


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
        self.rowcount = -1
        self.lastrowid = None
        self._native = None
        self._closed = False
        self._buffer = []
        self._position = 0

    @property
    def description(self):
        if self._native is None:
            return None
        return [(name, None, None, None, None, None, None) for name in self._native.columns] or None

    def execute(self, sql, parameters=()):
        if self._closed or self.connection._closed:
            raise ProgrammingError('Cursor or connection is closed')
        if self._native is not None:
            _call(self._native.close)
            self._native = None
        self._buffer = []
        self._position = 0
        if not sql.strip():
            raise ProgrammingError('SQL statement is empty')
        if not self.connection.autocommit and not self.connection._in_transaction:
            words = re.sub(r'\A(?:\s+|--[^\n]*(?:\n|$)|/\*.*?\*/)*', '',
                           sql, flags=re.DOTALL).split(None, 1)
            if not words:
                raise ProgrammingError('SQL statement is empty')
            if words[0].upper() in ['INSERT', 'UPDATE', 'DELETE', 'REPLACE']:
                self.connection.execute('BEGIN').close()
        self._native = _call(self.connection.native.execute, sql, tuple(parameters))
        self.rowcount = self._native.rowcount
        self.connection._in_transaction = self._native.in_transaction
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

    # Rows already fetched for iteration are served before the native cursor.
    def _take_buffer(self, size):
        rows = self._buffer[self._position:self._position + size]
        self._position += len(rows)
        if self._position >= len(self._buffer):
            self._buffer = []
            self._position = 0
        return rows

    def fetchone(self):
        self._check_result()
        if self._buffer:
            return self._take_buffer(1)[0]
        return _call(self._native.fetchone)

    def fetchmany(self, size=None):
        self._check_result()
        size = self.arraysize if size is None else size
        rows = self._take_buffer(size) if self._buffer else []
        if len(rows) < size:
            rows.extend(_call(self._native.fetchmany, size - len(rows)))
        return rows

    def fetchall(self):
        self._check_result()
        rows = self._take_buffer(len(self._buffer)) if self._buffer else []
        rows.extend(_call(self._native.fetchall))
        return rows

    def _check_result(self):
        if self._closed or self.connection._closed or self._native is None:
            raise ProgrammingError('Cursor has no active result')

    def close(self):
        if not self._closed:
            if self._native is not None:
                _call(self._native.close)
            self._buffer = []
            self._closed = True

    def __iter__(self):
        return self

    def __next__(self):
        if not self._buffer:
            self._check_result()
            self._buffer = _call(self._native.fetchmany, self.arraysize)
            self._position = 0
            if not self._buffer:
                raise StopIteration
        row = self._buffer[self._position]
        self._position += 1
        if self._position >= len(self._buffer):
            self._buffer = []
            self._position = 0
        return row

    def __enter__(self):
        return self

    def __exit__(self, exception_type, exception, traceback):
        self.close()

    def setinputsizes(self, sizes):
        pass

    def setoutputsize(self, size, column=None):
        pass
