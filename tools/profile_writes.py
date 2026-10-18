"""Attribute WeeWX writer time to DB-API operations on the native backend."""

import collections
import json
import statistics
import tempfile
import time
from pathlib import Path

import weewxdb
import weewx.manager
import weewx.schemas.wview_small


def main():
    samples = collections.defaultdict(list)
    original = weewxdb.Cursor.execute

    def measured(self, sql, parameters=()):
        start = time.perf_counter_ns()
        try:
            return original(self, sql, parameters)
        finally:
            samples[sql.split()[0].upper()].append((time.perf_counter_ns() - start) / 1e6)

    weewxdb.Cursor.execute = measured
    try:
        with tempfile.TemporaryDirectory() as directory:
            binding = {'driver': 'weewxdb.driver',
                       'database_name': str(Path(directory) / 'profile.wwdb')}
            with weewx.manager.DaySummaryManager.open_with_create(
                    binding, schema=weewx.schemas.wview_small.schema) as manager:
                samples.clear()
                start = time.perf_counter_ns()
                for index in range(1, 501):
                    manager.addRecord({
                        'dateTime': 1704067200 + index * 300, 'usUnits': 17, 'interval': 5,
                        'outTemp': float(index % 31 - 10), 'rain': 0.0,
                        'windSpeed': 2.0, 'windDir': 270.0, 'windGust': 4.0, 'windGustDir': 180.0},
                        log_success=False, log_failure=False)
                elapsed = (time.perf_counter_ns() - start) / 1e6
            report = {'records': 500, 'total_ms': elapsed, 'sql': {
                key: {'calls': len(values), 'total_ms': sum(values),
                      'median_ms': statistics.median(values)} for key, values in samples.items()}}
            print(json.dumps(report, indent=2))
    finally:
        weewxdb.Cursor.execute = original


if __name__ == '__main__':
    main()
