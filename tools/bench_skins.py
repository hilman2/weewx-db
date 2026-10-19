"""Time complete and repeated report runs of popular skins on SQLite or the Rust driver.

Each process measures one skin on one engine. Run the engines in separate
processes: the Rust driver registers its XType provider when it opens.
With --advance, a persistent connection adds one archive record before every
repeated run, as WeeWX does between report cycles. The databases are then
copied into the station directory first.
"""

import argparse
import json
import logging
import os
import resource
import shutil
import sys
import time
from pathlib import Path

import configobj

ROOT = Path(__file__).resolve().parents[1]
# Skin directory, Python search-list directory, report name and report options
# for each pinned checkout in references.json.
SKINS = {
    'Seasons': ('weewx-source/src/weewx_data/skins/Seasons', None, 'Seasons', {}),
    'Belchertown': ('belchertown-source/skins/Belchertown', 'belchertown-source/bin/user', 'Belchertown',
                    {'Extras': {'forecast_enabled': '0', 'aqi_enabled': '0', 'earthquake_enabled': '0',
                                'mqtt_websockets_enabled': '0', 'radar_html': ''}}),
    'neowx-material': ('neowx-material-source/src', None, 'neowx-material', {}),
    'weewx-wdc': ('wdc-source/skins/weewx-wdc', 'wdc-source/bin/user', 'WdcReport', {}),
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('engine', choices=['sqlite', 'rust'])
    parser.add_argument('skin', choices=sorted(SKINS))
    parser.add_argument('source', help='SQLite archive, used by the sqlite engine')
    parser.add_argument('database', help='imported Rust database, used by the rust engine')
    parser.add_argument('--runs', type=int, default=5, help='repeated runs after the complete first run')
    parser.add_argument('--references', type=Path, default=ROOT / 'build/weewx-db-trial')
    parser.add_argument('--station', type=Path, default=ROOT / 'build/skin-runs')
    parser.add_argument('--advance', action='store_true', help='add one archive record before each repeated run')
    parser.add_argument('--result-cache', type=int, help='driver option result_cache for the rust engine')
    options = parser.parse_args()
    skin_path, user_path, report_name, report_options = SKINS[options.skin]
    station = (options.station / ('%s-%s' % (options.skin, options.engine))).resolve()
    if station.exists():
        shutil.rmtree(station)
    (station / 'bin/user').mkdir(parents=True)
    (station / 'bin/user/__init__.py').write_text('')
    if user_path:
        for module in (options.references / user_path).glob('*.py'):
            shutil.copy(module, station / 'bin/user' / module.name)
    shutil.copytree(options.references / skin_path, station / 'skins' / options.skin)
    sys.path.insert(0, str(station / 'bin'))
    os.environ['TZ'] = 'UTC'
    time.tzset()
    logging.basicConfig(level=logging.WARNING, stream=sys.stderr)
    import weewx.manager
    import weewx.reportengine
    import weewx.station
    import weewx.units
    import weewx.wxxtypes
    import weewx.xtypes
    path = Path(options.source if options.engine == 'sqlite' else options.database).resolve()
    if options.advance:
        copy = station / 'data' / path.name
        copy.parent.mkdir()
        for source in path.parent.glob(path.name + '*'):
            shutil.copy(source, copy.parent / source.name)
        path = copy
    database = {'sqlite': {'driver': 'weedb.sqlite', 'database_name': str(path)},
                'rust': {'driver': 'weewxdb.driver', 'database_name': str(path)}}[options.engine]
    if options.result_cache is not None and options.engine == 'rust':
        database['result_cache'] = str(options.result_cache)
    # The installer's database_type tells Belchertown to use SQLite syntax.
    database['database_type'] = 'SQLite'
    report = dict(report_options, skin=options.skin, enable='true')
    config = configobj.ConfigObj({
        'WEEWX_ROOT': str(station), 'USER_ROOT': 'bin/user', 'debug': 0,
        'Station': {'location': 'Benchmark', 'latitude': '52.0', 'longitude': '13.0', 'altitude': ['40', 'meter'],
                    'station_type': 'Simulator', 'rain_year_start': '1', 'week_start': '6'},
        'StdConvert': {'target_unit': 'US'},
        'StdArchive': {'archive_interval': '300', 'data_binding': 'wx_binding'},
        'StdReport': {'SKIN_ROOT': 'skins', 'HTML_ROOT': 'public_html', 'log_success': 'False',
                      report_name: report},
        'DataBindings': {'wx_binding': {'database': 'archive', 'table_name': 'archive',
                                        'manager': 'weewx.manager.DaySummaryManager',
                                        'schema': 'weewx.schemas.wview_extended.schema'}},
        'DatabaseTypes': {'SQLite': {'driver': 'weedb.sqlite'}},
        'Databases': {'archive': database},
    })
    altitude = weewx.units.ValueTuple(40, 'meter', 'group_altitude')
    weewx.xtypes.xtypes.extend([weewx.wxxtypes.WXXTypes(altitude, 52.0, 13.0),
                               weewx.wxxtypes.ETXType(altitude, 52.0, 13.0)])
    # The writer stays open like WeeWX's main thread; reports open their own.
    writer = weewx.manager.open_manager_with_config(config, 'wx_binding')
    timestamp = writer.lastGoodStamp()
    station_info = weewx.station.StationInfo(**config['Station'])
    samples = []
    for run in range(options.runs + 1):
        if options.advance and run > 0:
            record = writer.getRecord(timestamp)
            timestamp = record['dateTime'] = timestamp + 300
            writer.addRecord(record)
        engine = weewx.reportengine.StdReportEngine(config, station_info, record=None, gen_ts=timestamp,
                                                    first_run=run == 0)
        began = time.perf_counter()
        engine.run()
        samples.append(time.perf_counter() - began)
    writer.close()
    files = sum(len(names) for _, _, names in os.walk(station / 'public_html'))
    # Peak resident memory of the whole process, in MiB.
    peak = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024
    print(json.dumps({'skin': options.skin, 'engine': options.engine, 'files': files, 'peak_rss_mib': round(peak),
                      'complete_s': round(samples[0], 3), 'repeated_s': [round(s, 3) for s in samples[1:]]}))


if __name__ == '__main__':
    main()
