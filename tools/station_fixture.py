"""Create a synthetic SQLite station archive in WeeWX's wview_extended schema."""

import argparse
import math
import os
import random
import sqlite3
import time

import weewx.manager
import weewx.schemas.wview_extended

COLUMNS = ('dateTime', 'usUnits', 'interval', 'barometer', 'pressure', 'altimeter', 'inTemp', 'outTemp',
           'inHumidity', 'outHumidity', 'windSpeed', 'windDir', 'windGust', 'windGustDir', 'rainRate',
           'rain', 'dewpoint', 'windchill', 'heatindex', 'appTemp', 'radiation', 'UV', 'ET')


def records(start, count):
    """Yield five-minute US-unit records with daily and seasonal cycles."""
    generator = random.Random(1)
    for index in range(count):
        time_ts = start + (index + 1) * 300
        day = (time_ts % 86400) / 86400.0
        season = math.sin((time_ts - start) / (365.25 * 86400) * 2 * math.pi - math.pi / 2)
        daily = math.sin((day - 0.375) * 2 * math.pi)
        temperature = 50 + 20 * season + 10 * daily + generator.uniform(-1, 1)
        humidity = max(15.0, min(100.0, 70 - 20 * daily + generator.uniform(-3, 3)))
        wind = max(0.0, 6 + 4 * math.sin(index / 37.0) + generator.uniform(-2, 2))
        direction = (index * 7) % 360
        # Rain falls around midday on every fifth day.
        rain = 0.01 if (index // 288) % 5 == 0 and 0.3 < day < 0.6 and index % 3 == 0 else 0.0
        radiation = max(0.0, 800 * math.sin((day - 0.25) * 2 * math.pi)) * (0.6 + 0.4 * season)
        barometer = 29.92 + 0.3 * math.sin(index / 2000.0)
        yield (time_ts, 1, 5, barometer, barometer - 0.92, barometer, 70.0, temperature, 45.0, humidity,
               wind, direction, wind + generator.uniform(0, 6), (direction + 20) % 360, rain * 12, rain,
               temperature - (100 - humidity) / 2.778, temperature if temperature < 50 else None,
               temperature if temperature > 80 else None, temperature, radiation, radiation / 100.0,
               0.005 if radiation > 0 else 0.0)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('destination')
    parser.add_argument('--years', type=int, default=10)
    parser.add_argument('--start', type=int, default=1704067200)
    options = parser.parse_args()
    if os.path.exists(options.destination):
        parser.error('destination exists')
    binding = {'driver': 'weedb.sqlite', 'database_name': os.path.abspath(options.destination)}
    weewx.manager.DaySummaryManager.open_with_create(binding, schema=weewx.schemas.wview_extended.schema).close()
    count = options.years * 365 * 288
    with sqlite3.connect(options.destination) as connection:
        connection.executemany('INSERT INTO archive (%s) VALUES (%s)' % (','.join(COLUMNS), ','.join('?' * len(COLUMNS))),
                               records(options.start, count))
    began = time.time()
    with weewx.manager.DaySummaryManager.open(binding) as manager:
        manager.backfill_day_summary()
    print('%d records; daily summaries built in %.1f s' % (count, time.time() - began))


if __name__ == '__main__':
    main()
