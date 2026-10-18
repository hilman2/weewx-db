"""Compare on-demand WeeWX XTypes with fused Rust calculations."""

import argparse
import json
import os
import platform
import statistics
import sys
import time

import weewx.manager
import weewx.units
import weewx.wxxtypes
import weewx.xtypes

from bench_native import equivalent, sample


def execution_details():
    affinity = sorted(os.sched_getaffinity(0)) if hasattr(os, 'sched_getaffinity') else None
    return {'cpu_affinity': affinity, 'platform': platform.platform(),
            'python': platform.python_version(), 'weewx_source': weewx.xtypes.__file__}


def compare(name, functions, rounds):
    timings = [[], []]
    first = [None, None]
    for iteration in range(rounds + 1):
        answers = [None, None]
        for index in ([0, 1] if iteration % 2 == 0 else [1, 0]):
            answers[index], elapsed = sample(functions[index])
            if iteration == 0:
                first[index] = elapsed
            else:
                timings[index].append(elapsed)
        assert equivalent(*answers), '%s answers differ' % name
        print('%s: round %s/%s verified' % (name, iteration, rounds), file=sys.stderr, flush=True)
    medians = [statistics.median(values) for values in timings]
    return {'workload': name, 'baseline_median_ms': medians[0],
            'native_median_ms': medians[1], 'speedup': medians[0] / medians[1],
            'baseline_samples_ms': timings[0], 'native_samples_ms': timings[1],
            'first_baseline_ms': first[0], 'first_native_ms': first[1],
            'verified_answers': rounds + 1}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('source')
    parser.add_argument('database')
    parser.add_argument('--rounds', type=int, default=3)
    parser.add_argument('--cache-mib', type=int, default=128)
    parser.add_argument('--workspace-mib', type=int, default=256)
    parser.add_argument('--scalar-years', type=int, default=10)
    options = parser.parse_args()
    if options.rounds < 1 or options.scalar_years < 1:
        parser.error('rounds and scalar-years must be positive')
    os.environ['TZ'] = 'UTC'
    time.tzset()
    bindings = [
        {'driver': 'weedb.sqlite', 'database_name': os.path.abspath(options.source),
         'pragmas': {'cache_size': -options.cache_mib * 1024, 'mmap_size': 0}},
        {'driver': 'weewxdb.driver', 'database_name': os.path.abspath(options.database),
         'cache_mib': options.cache_mib, 'workspace_mib': options.workspace_mib}]
    provider = weewx.wxxtypes.WXXTypes(
        weewx.units.ValueTuple(40, 'meter', 'group_altitude'), 52, 13)
    weewx.xtypes.xtypes.append(provider)
    managers = [weewx.manager.Manager.open(binding) for binding in bindings]
    try:
        first, last = managers[0].first_timestamp, managers[0].last_timestamp
        assert (first, last) == (managers[1].first_timestamp, managers[1].last_timestamp)
        for manager in managers:
            assert not {'dewpoint', 'humidex', 'heatindex'}.intersection(manager.sqlkeys), \
                'Use a fixture without stored derived observations'
        scalar_span = (max(first - 300, last - options.scalar_years * 365 * 86400), last)
        year_span = (max(first - 300, last - 365 * 86400), last)
        rows = managers[0].getSql('SELECT COUNT(*) FROM archive WHERE dateTime>? AND dateTime<=?', scalar_span)[0]
        cases = []
        for observation in ['dewpoint', 'humidex', 'heatindex']:
            cases.append(compare(observation + '_mean', [
                lambda manager=manager: weewx.xtypes.get_aggregate(
                    observation, scalar_span, 'avg', manager)
                for manager in managers], options.rounds))
            cases.append(compare(observation + '_hourly_year', [
                lambda manager=manager: weewx.xtypes.get_series(
                    observation, year_span, manager, aggregate_type='avg', aggregate_interval=3600)
                for manager in managers], options.rounds))
        # Both SQL variants use the same Rust formula and storage. The CTE
        # prevents the fused aggregate plan, exposing SQL row-dispatch cost.
        native = managers[1]
        query = 'SELECT AVG(humidex_c(outTemp,outHumidity)) FROM %s WHERE dateTime>? AND dateTime<=?'
        cases.append(compare('humidex_sql_fusion', [
            lambda: native.getSql('WITH records AS (SELECT * FROM archive) ' + query % 'records', scalar_span),
            lambda: native.getSql(query % 'archive', scalar_span)], options.rounds))
        print(json.dumps({'execution': execution_details(), 'rounds': options.rounds,
                          'cache_mib': options.cache_mib, 'workspace_mib': options.workspace_mib,
                          'scalar_rows': rows, 'scalar_span': scalar_span, 'hourly_span': year_span,
                          'cases': cases,
                          'note': 'On-demand derived observations, with no stored derived values '
                                  'or derived summaries. XType baseline is WeeWX Python over SQLite; '
                                  'SQL fusion baseline uses the same Rust UDF through the SQL row loop. '
                                  'First run is excluded from warm medians. Engine order alternates. '
                                  'Answers, units, NULLs and timestamps are checked after every pair '
                                  'with 1e-10 relative / 1e-9 absolute float tolerance. Warm OS cache; '
                                  'not a Raspberry Pi measurement.'}, indent=2))
    finally:
        for manager in managers:
            manager.close()
        weewx.xtypes.xtypes.remove(provider)


if __name__ == '__main__':
    main()
