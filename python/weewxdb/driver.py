"""Optional weedb driver using Rust storage and the SQLite SQL dialect."""

import functools
import os

import weedb
import weedb.sqlite

from . import connect as db_connect, IntegrityError, ProgrammingError, DatabaseError
from . import _weewxdb
from .native import RESULT_GENERATION


# weedb has DatabaseLockedError since WeeWX 5.5. Earlier releases have only
# its base class, which weewxd also answers by restarting the engine.
DatabaseLockedError = getattr(weedb, 'DatabaseLockedError', weedb.OperationalError)


def weedb_error(error):
    """Return the weedb exception for an error of this package or its Rust module.

    weewxd restarts its engine after a weedb.OperationalError and exits after
    anything that is not a weedb exception.
    """
    message = str(error)
    if isinstance(error, IntegrityError):
        return weedb.IntegrityError(message)
    if isinstance(error, ProgrammingError):
        return weedb.ProgrammingError(message)
    if 'no such table' in message:
        return weedb.NoTableError(message)
    if 'no such column' in message:
        return weedb.NoColumnError(message)
    if 'database is locked' in message:
        return DatabaseLockedError(message)
    if 'Permission denied' in message:
        return weedb.PermissionError(message)
    return weedb.OperationalError(message)


def guard(function):
    @functools.wraps(function)
    def guarded(*args, **kwargs):
        try:
            return function(*args, **kwargs)
        except (DatabaseError, _weewxdb.DatabaseError) as error:
            raise weedb_error(error) from error
    return guarded


class _Native:
    """The Rust connection as the XType, batch and packet adapters call it.

    Lock timeouts and operating-system errors raise weedb exceptions. Other
    errors stay ValueError, because the adapters read some of them as
    UnknownType or UnsupportedFeature.
    """

    def __init__(self, native):
        self._native = native

    def __getattr__(self, name):
        attribute = getattr(self._native, name)
        if callable(attribute):
            attribute = guard(attribute)
            # Later lookups find the wrapped method without __getattr__.
            setattr(self, name, attribute)
        return attribute


def _path(database_name, SQLITE_ROOT):
    return os.path.join(SQLITE_ROOT, database_name)


def create(database_name='', SQLITE_ROOT='', **options):
    path = _path(database_name, SQLITE_ROOT)
    if os.path.exists(path):
        raise weedb.DatabaseExistsError(path)
    os.makedirs(os.path.dirname(os.path.abspath(path)), exist_ok=True)
    try:
        _weewxdb.create(path)
    except ValueError as error:
        raise weedb_error(error) from error


def connect(database_name='', SQLITE_ROOT='', **options):
    return Connection(database_name, SQLITE_ROOT, **options)


def drop(database_name='', SQLITE_ROOT='', **options):
    path = _path(database_name, SQLITE_ROOT)
    if not os.path.isfile(path):
        raise weedb.NoDatabaseError(path)
    try:
        _weewxdb.drop_database(path)
    except ValueError as error:
        raise weedb_error(error) from error


class Connection(weedb.Connection):
    @guard
    def __init__(self, database_name='', SQLITE_ROOT='', **options):
        self.file_path = _path(database_name, SQLITE_ROOT)
        if not os.path.isfile(self.file_path):
            raise weedb.NoDatabaseError(self.file_path)
        connection = db_connect(self.file_path, timeout=float(options.get('timeout', 5)),
                                     cache_mib=int(options.get('cache_mib', 32)),
                                     workspace_mib=int(options.get('workspace_mib', 32)),
                                     sync_mode=options.get('sync_mode', 'always'))
        super().__init__(connection, database_name, 'sqlite')
        self.native = _Native(connection.native)
        # Aggregate results kept per generation for this database; 0 disables.
        self.result_cache = int(options.get('result_cache', RESULT_GENERATION))
        from . import xtypes
        xtypes.register()

    @guard
    def cursor(self):
        return Cursor(self.connection.cursor())

    @guard
    def tables(self):
        with self.connection.execute("SELECT name FROM sqlite_master WHERE type='table'") as cursor:
            return [row[0] for row in cursor]

    @guard
    def genSchemaOf(self, table):
        with self.connection.execute('PRAGMA table_info(%s)' % _quote(table)) as cursor:
            rows = cursor.fetchall()
        if not rows:
            raise weedb.NoTableError(table)
        for index, name, declared, not_null, default, primary in rows:
            yield (index, name, 'STR' if declared.startswith('CHAR') else declared,
                   not bool(not_null), default, bool(primary) or name == 'dateTime')

    def columnsOf(self, table):
        return [column[1] for column in self.genSchemaOf(table)]

    @property
    def has_math(self):
        return True

    @guard
    def begin(self):
        self.connection.execute('BEGIN TRANSACTION').close()

    @guard
    def commit(self):
        self.connection.commit()

    @guard
    def rollback(self):
        self.connection.rollback()

    @guard
    def close(self):
        self.connection.close()

    @guard
    def get_variable(self, name):
        if name == 'journal_mode':
            return name, 'weewx-wal'
        with self.connection.execute('PRAGMA %s' % name) as cursor:
            row = cursor.fetchone()
            return (name, row[0]) if row else None

    # WeeWX 5.3 added get_group_by and calls it only from then on.
    if hasattr(weedb.sqlite.Connection, 'get_group_by'):
        get_group_by = staticmethod(weedb.sqlite.Connection.get_group_by)


def _quote(name):
    return '"%s"' % name.replace('"', '""')


class Cursor(weedb.Cursor):
    def __init__(self, cursor):
        self.cursor = cursor

    @guard
    def execute(self, *args, **kwargs):
        self.cursor.execute(*args, **kwargs)
        return self

    @property
    def rowcount(self):
        return self.cursor.rowcount

    @guard
    def fetchone(self):
        return self.cursor.fetchone()

    @guard
    def fetchall(self):
        return self.cursor.fetchall()

    @guard
    def fetchmany(self, size=200):
        return self.cursor.fetchmany(size)

    def __iter__(self):
        return self

    @guard
    def __next__(self):
        return next(self.cursor)

    def close(self):
        self.cursor.close()

    def __enter__(self):
        return self

    def __exit__(self, exception_type, exception, traceback):
        self.close()
