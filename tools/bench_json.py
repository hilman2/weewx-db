"""Compare complete JSONGenerator rebuilds, incremental updates and idle runs."""

import argparse
import json
import os
import statistics
import sys
import tempfile
import time
from pathlib import Path

import configobj
import weewx.defaults
import weewx.jsongenerator
import weewx.manager
import weewx.station
import weewx.units
import weewx.wxxtypes
import weewx.xtypes

from bench_native import equivalent


def outputs(directory):
    result = {}
    for path in sorted(directory.rglob('*.json')):
        value = json.loads(path.read_text())
        if path.name == 'index.json':
            value.pop('rebuilt', None)
        result[str(path.relative_to(directory))] = value
    return result


def generator(binding, output, timestamp, observations=None, generator_class=None):
    config = configobj.ConfigObj({
        'WEEWX_ROOT': '/',
        'DataBindings': {'wx_binding': {'database': 'trial',
                                       'manager': 'weewx.manager.DaySummaryManager'}},
        'Databases': {'trial': binding}}, interpolation=False)
    weewx.defaults.defaults.interpolation = False
    skin = configobj.ConfigObj(weewx.defaults.defaults.dict(), interpolation=False)
    skin.merge({'HTML_ROOT': str(output), 'REPORT_NAME': 'DatabaseTrial',
                'data_binding': 'wx_binding', 'JSONGenerator': {
                    'include_daynight': False,
                    'JSONFiles': {'temperature': {'outTemp': {}},
                                  'rain': {'aggregate_type': 'sum', 'rain': {}},
                                  'wind': {'windSpeed': {}}},
                    'Archive': {'years': 2, 'months': 2, 'days': 7,
                                'year_resolution': 3600, 'old_year_resolution': '4h',
                                'month_resolution': 900, 'day_resolution': 0,
                                'extremes': [], 'budget': 0, 'rebuild': 0}}})
    if observations:
        skin['JSONGenerator']['JSONFiles'] = {
            observation: {observation: {}}
            for observation in observations}
    return (generator_class or weewx.jsongenerator.JSONGenerator)(
        config, configobj.ConfigObj(skin.dict(), interpolation=False), timestamp, True,
        weewx.station.StationInfo(latitude=52, longitude=13, altitude=[40, 'meter']))


def execution_details():
    affinity = sorted(os.sched_getaffinity(0)) if hasattr(os, 'sched_getaffinity') else None
    result = {'cpu_affinity': affinity}
    if affinity is not None and len(affinity) == 1:
        base = Path('/sys/devices/system/cpu/cpu%s' % affinity[0])
        for key, name in [('cpu_max_frequency_khz', 'cpufreq/cpuinfo_max_freq'),
                          ('smt_siblings', 'topology/thread_siblings_list')]:
            try:
                result[key] = (base / name).read_text().strip()
            except OSError:
                pass
    tasks = Path('/proc/self/task')
    if tasks.is_dir() and affinity is not None:
        result['thread_affinities'] = {}
        for task in tasks.iterdir():
            try:
                result['thread_affinities'][task.name] = sorted(os.sched_getaffinity(int(task.name)))
            except ProcessLookupError:
                pass
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('source')
    parser.add_argument('database')
    parser.add_argument('--rounds', type=int, default=3)
    parser.add_argument('--cache-mib', type=int, default=32)
    parser.add_argument('--workspace-mib', type=int, default=64)
    parser.add_argument('--source-ref', default='unspecified')
    parser.add_argument('--batch', action='store_true', help='Use the extension JSON adapter for the Rust backend')
    parser.add_argument('--extremes', action='store_true', help='Include per-bin min/max for each observation')
    parser.add_argument('--computed', action='store_true',
                        help='Generate on-demand dewpoint, humidex and heatindex instead of stored sensors')
    parser.add_argument('--observations', help='Comma-separated derived observations to generate')
    options = parser.parse_args()
    observations = options.observations.split(',') if options.observations else ['dewpoint', 'humidex', 'heatindex'] if options.computed else None
    computed = bool(observations)
    os.environ['TZ'] = 'UTC'
    time.tzset()
    bindings = [
        {'driver': 'weedb.sqlite', 'database_name': os.path.abspath(options.source),
         'pragmas': {'cache_size': -options.cache_mib * 1024, 'mmap_size': 0}},
        {'driver': 'weewxdb.driver', 'database_name': os.path.abspath(options.database),
         'cache_mib': options.cache_mib, 'workspace_mib': options.workspace_mib}]
    with weewx.manager.Manager.open(bindings[0]) as manager:
        last = manager.last_timestamp
        if computed:
            assert not set(observations).intersection(manager.sqlkeys)
    if computed:
        weewx.xtypes.xtypes.append(weewx.wxxtypes.WXXTypes(
            weewx.units.ValueTuple(40, 'meter', 'group_altitude'), 52, 13))
        weewx.xtypes.xtypes.append(weewx.wxxtypes.ETXType(
            weewx.units.ValueTuple(40, 'meter', 'group_altitude'), 52, 13))
    samples = {phase: [[], []] for phase in ['rebuild', 'incremental', 'idle']}
    count = 0
    byte_count = 0
    for iteration in range(options.rounds):
        with tempfile.TemporaryDirectory(prefix='weewxdb-json-') as directory:
            roots = [Path(directory) / 'sqlite', Path(directory) / 'weewxdb']
            if options.batch:
                from weewxdb.json import JSONGenerator
            instances = [generator(binding, output, last - 300, observations,
                                   JSONGenerator if options.batch and i == 1 else None)
                         for i, (binding, output) in enumerate(zip(bindings, roots))]
            if options.extremes:
                for instance in instances:
                    instance.skin_dict['JSONGenerator']['Archive']['extremes'] = observations or ['outTemp', 'rain', 'windSpeed']
            try:
                for phase in samples:
                    for index in ([0, 1] if iteration % 2 == 0 else [1, 0]):
                        instance = instances[index]
                        if phase != 'rebuild':
                            instance.gen_ts = last
                            instance.first_run = False
                        start = time.perf_counter_ns()
                        instance.start()
                        samples[phase][index].append((time.perf_counter_ns() - start) / 1e6)
                    expected, actual = (outputs(root) for root in roots)
                    assert expected, 'JSONGenerator did not write any files'
                    assert (equivalent(expected, actual) if computed else expected == actual), \
                        '%s output mismatch' % phase
                    count = len(actual)
                    byte_count = sum(p.stat().st_size for p in roots[1].rglob('*.json'))
                    if computed:
                        print('Computed JSON %s: round %s/%s verified' % (
                            phase, iteration + 1, options.rounds), file=sys.stderr, flush=True)
            finally:
                for instance in instances:
                    instance.finalize()
    cases = []
    for phase, timings in samples.items():
        baseline, native = map(statistics.median, timings)
        cases.append({'workload': phase, 'sqlite_median_ms': baseline,
                      'weewxdb_median_ms': native, 'speedup': baseline / native,
                      'sqlite_samples_ms': timings[0], 'weewxdb_samples_ms': timings[1]})
    print(json.dumps({'generator_ref': options.source_ref,
                      'generator_source': weewx.jsongenerator.__file__, 'batch_adapter': options.batch, 'extremes': options.extremes,
                      'execution': execution_details(),
                      'cache_mib': options.cache_mib, 'workspace_mib': options.workspace_mib,
                      'rounds': options.rounds, 'json_files': count, 'json_bytes': byte_count,
                      'computed_observations': observations if computed else False,
                      'cases': cases,
                      'note': 'Complete JSONGenerator.start timings include database access, '
                              'unit conversion and file serialization. All parsed JSON outputs '
                              'match after removing index.rebuilt. Computed mode uses 1e-10 relative / '
                              '1e-9 absolute float tolerance; stored mode requires exact equality. '
                              'Three sensors, ten-year '
                              'fixture, UTC, warm OS cache; not a Raspberry Pi measurement.'}, indent=2))


if __name__ == '__main__':
    main()
