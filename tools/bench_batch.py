"""Compare separate WeeWX calls with fused queries on the same archive."""

import argparse
import json
import os
import statistics
import time
from pathlib import Path

import weeutil.weeutil
import weewx.manager
import weewx.units
import weewx.wxxtypes
import weewx.xtypes
from weewxdb.batch import get_series_many
from bench_native import equivalent
from bench_json import execution_details


def measure(function, rounds):
    samples = []
    value = None
    for _ in range(rounds):
        begin = time.perf_counter_ns()
        value = function()
        samples.append((time.perf_counter_ns() - begin) / 1e6)
    return value, samples


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('source')
    parser.add_argument('database')
    parser.add_argument('--rounds', type=int, default=3)
    parser.add_argument('--cache-mib', type=int, default=32)
    parser.add_argument('--workspace-mib', type=int, default=32)
    args = parser.parse_args()
    os.environ['TZ'] = 'UTC'; time.tzset()
    bindings = [{'driver': 'weedb.sqlite', 'database_name': str(Path(args.source).resolve()),
                 'pragmas': {'cache_size': -args.cache_mib * 1024, 'mmap_size': 0}},
                {'driver': 'weewxdb.driver', 'database_name': str(Path(args.database).resolve()),
                 'cache_mib': args.cache_mib, 'workspace_mib': args.workspace_mib}]
    managers = [weewx.manager.DaySummaryManager.open(binding) for binding in bindings]
    altitude = weewx.units.ValueTuple(40, 'meter', 'group_altitude')
    weewx.xtypes.xtypes.extend([weewx.wxxtypes.WXXTypes(altitude, 52, 13),
                               weewx.wxxtypes.ETXType(altitude, 52, 13)])
    span = weeutil.weeutil.TimeSpan(managers[0].last_timestamp - 365 * 86400, managers[0].last_timestamp)
    requests = [{'observation': obs, 'span': span, 'aggregate': aggregate, 'interval': 3600}
                for obs in ['ET', 'appTemp', 'windchill'] for aggregate in ['avg', 'min', 'max']]
    statistics_out = {}
    def separate(manager):
        providers = weewx.xtypes.xtypes
        if getattr(manager.connection, 'native', None) is None:
            from weewxdb.xtypes import NativeXType
            weewx.xtypes.xtypes = [p for p in providers if not isinstance(p, NativeXType)]
        try:
            return [weewx.xtypes.get_series(r['observation'], span, manager, r['aggregate'], r['interval']) for r in requests]
        finally:
            weewx.xtypes.xtypes = providers
    try:
        expected, sqlite = measure(lambda: separate(managers[0]), args.rounds)
        legacy, rust = measure(lambda: separate(managers[1]), args.rounds)
        batched, batch = measure(lambda: get_series_many(requests, managers[1], statistics=statistics_out), args.rounds)
        assert equivalent(expected, legacy) and equivalent(expected, batched), 'Series differ from WeeWX'
        print(json.dumps({'execution': execution_details(), 'rounds': args.rounds,
                          'observations': ['ET', 'appTemp', 'windchill'], 'aggregates': ['avg', 'min', 'max'],
                          'interval_seconds': 3600, 'span': list(span),
                          'cache_mib': args.cache_mib, 'workspace_mib': args.workspace_mib,
                          'sqlite_samples_ms': sqlite, 'rust_separate_samples_ms': rust, 'rust_batch_samples_ms': batch,
                          'sqlite_median_ms': statistics.median(sqlite),
                          'rust_separate_median_ms': statistics.median(rust),
                          'rust_batch_median_ms': statistics.median(batch), 'batch_counters': statistics_out,
                          'verified': True, 'note': 'Warm OS cache. Numeric tolerance 1e-10 relative / 1e-9 absolute. Not a Raspberry Pi measurement.'}, indent=2))
    finally:
        for manager in managers:
            manager.close()


if __name__ == '__main__':
    main()
