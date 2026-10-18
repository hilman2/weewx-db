"""Compare WeeWX queries and durable manager writes through both drivers."""

import argparse
import json
import math
import os
import platform
import resource
import statistics
import tempfile
import time
import logging
from pathlib import Path

import weewx
import weewx.manager
import weewx.schemas.wview_small
import weewx.xtypes


def equivalent(left, right):
    if isinstance(left, dict):
        return left.keys() == right.keys() and all(equivalent(value, right[key])
                                                  for key, value in left.items())
    if isinstance(left, (tuple, list)):
        return len(left) == len(right) and all(equivalent(a, b) for a, b in zip(left, right))
    if isinstance(left, float):
        return math.isclose(left, right, rel_tol=1e-10, abs_tol=1e-9)
    return left == right


def sample(function):
    start = time.perf_counter_ns()
    result = function()
    return result, (time.perf_counter_ns() - start) / 1e6


def timed_case(name, baseline, native, rounds):
    expected, first_sqlite = sample(baseline)
    actual, first_native = sample(native)
    assert equivalent(expected, actual), name
    baseline()
    native()
    timings = [[], []]
    for iteration in range(rounds):
        answers = [None, None]
        for index in ([0, 1] if iteration % 2 == 0 else [1, 0]):
            answers[index], elapsed = sample([baseline, native][index])
            timings[index].append(elapsed)
        assert equivalent(*answers), name
    medians = [statistics.median(values) for values in timings]
    return {'workload': name, 'sqlite_median_ms': medians[0], 'weewxdb_median_ms': medians[1],
            'speedup': medians[0] / medians[1], 'first_sqlite_ms': first_sqlite,
            'first_weewxdb_ms': first_native, 'verified_answers': rounds + 1}


def writes(count):
    results = []
    with tempfile.TemporaryDirectory(prefix='weewxdb-bench-') as directory:
        for label, driver, pragmas, sync_mode in [
                ('sqlite_delete_full', 'weedb.sqlite', {'synchronous': 'FULL'}, 'always'),
                ('sqlite_wal_full', 'weedb.sqlite', {'journal_mode': 'WAL', 'synchronous': 'FULL'}, 'always'),
                ('weewxdb_always', 'weewxdb.driver', {}, 'always'),
                ('weewxdb_background', 'weewxdb.driver', {}, 'background')]:
            path = Path(directory) / label
            binding = {'driver': driver, 'database_name': str(path), 'pragmas': pragmas,
                       'sync_mode': sync_mode}
            manager = weewx.manager.DaySummaryManager.open_with_create(
                binding, schema=weewx.schemas.wview_small.schema)
            timings = []
            for index in range(1, count + 1):
                record = {'dateTime': 1704067200 + index * 300, 'usUnits': weewx.METRICWX,
                          'interval': 5, 'outTemp': float(index % 31 - 10), 'rain': 0.0,
                          'windSpeed': 2.0, 'windDir': 270.0, 'windGust': 4.0, 'windGustDir': 180.0}
                _, elapsed = sample(lambda: manager.addRecord(record, log_success=False, log_failure=False))
                timings.append(elapsed)
            assert len(list(manager.genBatchRecords())) == count
            actual = weewx.xtypes.get_aggregate('outTemp',(1704067200,1704067200+count*300),'avg',manager)
            expected = sum(float(index % 31 - 10) for index in range(1,count+1))/count
            assert equivalent(actual[0],expected), (label,actual,expected)
            for observation in ['outTemp','windSpeed']:
                assert manager.getSql('SELECT COUNT(*) FROM archive_day_%s' % observation)[0] > 0
            before = sum(p.stat().st_size for p in Path(directory).glob(label + '*'))
            checkpoint_ms = None
            if driver == 'weewxdb.driver':
                _, checkpoint_ms = sample(manager.connection.native.checkpoint)
            else:
                with manager.connection.cursor() as cursor:
                    cursor.execute('PRAGMA wal_checkpoint(TRUNCATE)')
            after = sum(p.stat().st_size for p in Path(directory).glob(label + '*'))
            manager.close()
            with weewx.manager.DaySummaryManager.open(binding) as restored:
                assert len(list(restored.genBatchRecords())) == count
            results.append({'engine': label, 'commits': count,
                            'median_ms': statistics.median(timings),
                            'p95_ms': sorted(timings)[math.ceil(count * .95) - 1],
                            'total_ms': sum(timings), 'checkpoint_ms': checkpoint_ms,
                            'bytes_before_checkpoint': before, 'bytes_after_checkpoint': after})
    return results


def main():
    # WeeWX logs caught daily-summary SQL errors; turn those into failed
    # measurements instead of timing an incomplete archive update.
    class FailOnDatabaseError(logging.Handler):
        def emit(self, record):
            if record.levelno >= logging.ERROR:
                raise RuntimeError(record.getMessage())

    logging.getLogger('weewx.manager').addHandler(FailOnDatabaseError())
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('source')
    parser.add_argument('database')
    parser.add_argument('--cache-mib', type=int, default=32)
    parser.add_argument('--workspace-mib', type=int, default=32)
    parser.add_argument('--rounds', type=int, default=7)
    parser.add_argument('--write-records', type=int, default=200)
    options = parser.parse_args()
    os.environ['TZ'] = 'UTC'
    time.tzset()
    sqlite_binding = {'driver': 'weedb.sqlite', 'database_name': options.source,
                      'pragmas': {'cache_size': -options.cache_mib * 1024, 'mmap_size': 0}}
    native_binding = {'driver': 'weewxdb.driver', 'database_name': options.database,
                      'cache_mib': options.cache_mib, 'workspace_mib': options.workspace_mib}
    managers = [weewx.manager.DaySummaryManager.open(binding)
                for binding in [sqlite_binding, native_binding]]
    first, last = managers[0].first_timestamp, managers[0].last_timestamp
    assert (first, last) == (managers[1].first_timestamp, managers[1].last_timestamp)
    cases = []
    tasks = [
        ('weewx_raw_month', lambda m: weewx.xtypes.get_series('outTemp', (last - 30 * 86400, last), m)),
        ('weewx_hourly_year', lambda m: weewx.xtypes.get_series('outTemp', (last - 365 * 86400, last), m,
                                                            aggregate_type='avg', aggregate_interval=3600)),
        ('weewx_daily_year_mean', lambda m: weewx.xtypes.get_aggregate('outTemp', (last - 365 * 86400, last), 'avg', m)),
        ('sql_group_hourly_year', lambda m: list(m.genSql(
            'SELECT CAST(dateTime/3600 AS INTEGER)*3600,AVG(outTemp) FROM archive '
            'WHERE dateTime>? AND dateTime<=? GROUP BY 1 ORDER BY 1', (last - 365 * 86400, last)))),
    ]
    try:
        for name, task in tasks:
            if name.startswith('sql_group'):
                def native_group():
                    return list(managers[1].genSql(
                        'SELECT time_bucket(dateTime,3600),AVG(outTemp) FROM archive '
                        'WHERE dateTime>? AND dateTime<=? GROUP BY 1 ORDER BY 1',
                        (last - 365 * 86400, last)))
                native_task = native_group
            else:
                native_task = lambda task=task: task(managers[1])
            cases.append(timed_case(name, lambda task=task: task(managers[0]), native_task, options.rounds))
    finally:
        for manager in managers:
            manager.close()
    report = {'platform': platform.platform(), 'python': platform.python_version(),
              'cache_mib': options.cache_mib, 'workspace_mib': options.workspace_mib,
              'rounds': options.rounds, 'cases': cases,
              'writes': writes(options.write_records) if options.write_records else [],
              'weewx_source': weewx.__file__,
              'peak_rss_kib': resource.getrusage(resource.RUSAGE_SELF).ru_maxrss,
              'note': 'Warm OS cache, alternating driver order, comparisons outside timing. '
                      'Writes use actual DaySummaryManager.addRecord, one transaction per record, '
                      'including daily summaries. Always modes sync before acknowledgement; background '
                      'allows loss of the newest transaction. Not full report rendering. '
                      'RSS covers Python and both engines.'}
    print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
