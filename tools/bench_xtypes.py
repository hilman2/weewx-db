"""Measure windowed weather series, historical xaggs and solar/lunar work."""

import argparse
import importlib.util
import json
import math
import os
import sys
import time

import ephem
import weewx.almanac
import weewx.manager
import weewx.units
import weewx.wxxtypes
import weewx.xtypes
import weewxdb

from bench_computed import compare, execution_details
from bench_native import equivalent


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('source')
    parser.add_argument('database')
    parser.add_argument('--rounds', type=int, default=3)
    parser.add_argument('--cache-mib', type=int, default=128)
    parser.add_argument('--workspace-mib', type=int, default=256)
    parser.add_argument('--xaggs-source', default='build/weewx-db-trial/xaggs-source/bin/user/xaggs.py')
    options = parser.parse_args()
    os.environ['TZ'] = 'UTC'
    time.tzset()
    altitude = weewx.units.ValueTuple(40, 'meter', 'group_altitude')
    weewx.xtypes.xtypes.extend([weewx.wxxtypes.WXXTypes(altitude, 52, 13),
                               weewx.wxxtypes.ETXType(altitude, 52, 13)])
    spec = importlib.util.spec_from_file_location('reference_xaggs', options.xaggs_source)
    xaggs = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = xaggs
    spec.loader.exec_module(xaggs)
    weewx.xtypes.xtypes.extend([xaggs.XAggsHistorical(), xaggs.XAggsAvg()])
    bindings = [{'driver': 'weedb.sqlite', 'database_name': os.path.abspath(options.source),
                 'pragmas': {'cache_size': -options.cache_mib * 1024, 'mmap_size': 0}},
                {'driver': 'weewxdb.driver', 'database_name': os.path.abspath(options.database),
                 'cache_mib': options.cache_mib, 'workspace_mib': options.workspace_mib}]
    managers = [weewx.manager.DaySummaryManager.open(binding) for binding in bindings]
    # The SQLite side must use the original almanac, including solar XTypes.
    # Native weather series call Rust astronomy directly.
    weewx.almanac.almanacs[:] = [weewx.almanac.PyEphemAlmanacType()]
    try:
        first, last = managers[0].first_timestamp, managers[0].last_timestamp
        assert (first, last) == (managers[1].first_timestamp, managers[1].last_timestamp)
        year = (last - 365 * 86400, last)
        week = (last - 7 * 86400, last)
        cases = []
        for observation, span in [('ET', year), ('appTemp', year), ('maxSolarRad', week)]:
            assert observation not in managers[0].sqlkeys
            cases.append(compare(observation + '_hourly', [
                lambda manager=manager, observation=observation, span=span: weewx.xtypes.get_series(
                    observation, span, manager, aggregate_type='avg', aggregate_interval=3600)
                for manager in managers], options.rounds))
        history = (1704067200, 1704067200 + 365 * 86400)
        cases.append(compare('historical_avg_daily_year', [
            lambda manager=manager: weewx.xtypes.get_series('outTemp', history, manager,
                aggregate_type='historical_avg', aggregate_interval=86400)
            for manager in managers], options.rounds))
        site = ephem.Observer()
        site.lat, site.lon = math.radians(52), math.radians(13)

        def baseline_positions():
            result = []
            for i in range(366):
                site.date = (1704110400 + i * 86400) / 86400 + 25567.5
                for body in [ephem.Sun, ephem.Moon]:
                    position = body(site)
                    result.append([float(position.ra), float(position.dec)])
            return result

        def native_positions():
            result = []
            for i in range(366):
                for body in ['sun', 'moon']:
                    position = weewxdb.astronomy(1704110400 + i * 86400, 52, 13, body=body)
                    result.append([math.radians(position['right_ascension']), math.radians(position['declination'])])
            return result

        # Equatorial coordinates avoid PyEphem's f32 alt/az rounding. Check
        # those separately with the physically appropriate tolerance in tests.
        assert equivalent(baseline_positions(), native_positions())
        cases.append(compare('sun_moon_positions_366_days', [baseline_positions, native_positions], options.rounds))
        print(json.dumps({'execution': execution_details(), 'rounds': options.rounds,
                          'cache_mib': options.cache_mib, 'workspace_mib': options.workspace_mib,
                          'archive_rows': managers[0].getSql('SELECT COUNT(*) FROM archive')[0],
                          'cases': cases,
                          'note': 'Warm OS cache, one excluded warmup, alternating engine order. '
                                  'All paired answers checked after timing with 1e-10 relative / '
                                  '1e-9 absolute tolerance. ET has a one-hour sliding window. '
                                  'Solar XType covers seven days; other weather series cover 365 days. '
                                  'Historical xaggs uses 365 daily queries over all years. '
                                  'Astronomy baseline is PyEphem, whose core is already C. '
                                  'Not a Raspberry Pi measurement.'}, indent=2))
    finally:
        for manager in managers:
            manager.close()


if __name__ == '__main__':
    main()
