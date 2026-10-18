# Desktop measurements, 18 October 2026

These measurements use a synthetic ten-year archive with 1,051,200
five-minute records, 32 observation columns and 16 active sensors. The host
is Linux x86_64 on an AMD Ryzen AI 9 HX 470 with NVMe storage, Rust 1.99.0
and Python 3.13.16. They are not Raspberry Pi measurements.

## Complete JSON generation

The JSON generator and its WeeWX dependencies come from
`perf/horizon-json-streaming` at commit `28ac6ab8`. That version already
groups SQLite archive queries. This compares the Rust driver against that
optimized implementation, including query execution, unit conversion and
JSON file writing.

The report contains outTemp, rain and windSpeed, annual history at one-hour
or four-hour resolution, two recent months at fifteen-minute resolution and
seven recent days at archive resolution. It produces 56 JSON files totaling
664,499 bytes. Every parsed JSON document matched exactly in every run after
removing the index's wall-clock `rebuilt` timestamp.

Warm medians of seven runs, in milliseconds:

| Cache / native workspace | Phase | SQLite | Rust backend | SQLite time / Rust time |
| --- | --- | ---: | ---: | ---: |
| 4 / 16 MiB | Rebuild | 692.236 | 301.026 | 2.30 |
| 4 / 16 MiB | Incremental update | 29.441 | 29.211 | 1.01 |
| 4 / 16 MiB | No new data | 1.158 | 1.362 | 0.85 |
| 128 / 256 MiB | Rebuild | 730.777 | 296.702 | 2.46 |
| 128 / 256 MiB | Incremental update | 28.958 | 29.605 | 0.98 |
| 128 / 256 MiB | No new data | 1.089 | 1.293 | 0.84 |

The rebuild benefits from the Rust time-series path. This fixture does not
show an incremental-update improvement. Increasing the cache budget does
not materially change Rust's rebuild time.

Raw reports contain all samples:
[4 MiB](v2-json-4mib.json), [128 MiB](v2-json-128mib.json).
Each round starts in a new output directory, then advances generation by one
five-minute record and repeats without another timestamp change. Engine
order alternates. The OS file cache is not cleared. Output comparison and
temporary-directory cleanup are outside the timed section.

## JSON generation on one slower CPU

The complete benchmark process was pinned to logical CPU 6 with `taskset -c 6`.
Its reported maximum frequency is 3,351,352 kHz, compared with 5,297,298 kHz
on CPUs 0 through 3. Its SMT sibling, CPU 18, was outside the process affinity
mask. The report records the process and observed thread masks as `[6]`.
The frequency is a reported maximum, not a fixed clock during the run.

The dataset, generator, output configuration and seven-round method are the
same as above. Both engines run on the selected CPU. Warm medians in ms:

| Cache / native workspace | Phase | SQLite | Rust backend | SQLite time / Rust time |
| --- | --- | ---: | ---: | ---: |
| 4 / 16 MiB | Rebuild | 1105.933 | 478.091 | 2.31 |
| 4 / 16 MiB | Incremental update | 45.928 | 47.486 | 0.97 |
| 4 / 16 MiB | No new data | 2.893 | 2.530 | 1.14 |
| 128 / 256 MiB | Rebuild | 1126.111 | 469.967 | 2.40 |
| 128 / 256 MiB | Incremental update | 46.312 | 47.102 | 0.98 |
| 128 / 256 MiB | No new data | 2.851 | 2.465 | 1.16 |

Raw reports include affinity metadata and all samples:
[4 MiB, CPU 6](v2-json-4mib-cpu6.json),
[128 MiB, CPU 6](v2-json-128mib-cpu6.json).
This constrains CPU placement and parallelism; it remains an x86/NVMe test.

Reproduce the pinned run after the setup below:

```sh
taskset -c 6 env PYTHONPATH=build/weewx-db-trial/json-source/src python3 \
    experimental/weewx-db/tools/bench_json.py build/weather.sdb build/weather.wwdb \
    --cache-mib 128 --workspace-mib 256 --rounds 7 --source-ref 28ac6ab8
```

## On-demand computed XTypes

The computed fixture adds outHumidity to a copy of the archive above. Humidity
varies from 25 to 95 percent, with NULL every 41st record. No dewpoint, humidex
or heatindex column or daily summary exists. Both engines therefore calculate
the derived readings at query time. These measurements use CPU 6, a 128 MiB
cache and 256 MiB native workspace, with the same affinity restriction as above.

The microbenchmark uses WeeWX at `def717274ef07da1910f9a0975f8d76ed287aa47`.
The baseline is WeeWX's existing XTypeTable and WXXTypes over SQLite. The
Rust driver selects the native formula automatically through the same XType
API. Scalar queries scan all 1,051,200 archive rows across ten years and
average the valid derived readings.
Hourly series cover the last 365 days. Warm medians of three runs in ms,
excluding one initial run and alternating engine order:

| Calculation | SQLite and Python | Rust backend | Baseline time / Rust time |
| --- | ---: | ---: | ---: |
| Ten-year dewpoint mean | 6708.842 | 15.203 | 441.30 |
| Ten-year humidex mean | 6860.915 | 27.547 | 249.06 |
| Ten-year heatindex mean | 6814.402 | 9.325 | 730.76 |
| Annual hourly dewpoint | 852.507 | 23.689 | 35.99 |
| Annual hourly humidex | 875.927 | 25.466 | 34.40 |
| Annual hourly heatindex | 862.808 | 23.082 | 37.38 |

Every pair checks numerical answers with relative tolerance 1e-10 and absolute
tolerance 1e-9, plus units, NULLs and timestamps. The large ratios include
eliminating Python archive-record construction and per-record XType dispatch.
They do not isolate the storage format. More arithmetic does not necessarily
produce a larger ratio. The temperate fixture seldom reaches heatindex's
full polynomial, while humidex always needs additional transcendental work.

A separate control uses the same Rust humidex function and custom storage
on both sides. Calling the function through SQL's per-row loop takes
328.637 ms; the fused native aggregate takes 26.438 ms, a factor of 12.43.
The CTE baseline prevents the native aggregate plan. This control isolates
the benefit of the execution path after the formula is already in Rust.
All samples are in [the computed-XType report](v2-computed-128mib-cpu6.json).

The complete JSON generator at `28ac6ab8` was also run with the three derived
observations replacing the three stored observations. Its archive configuration
and three-phase method are the same as above. Three-round medians in ms:

| Phase | SQLite and Python | Rust backend | Baseline time / Rust time |
| --- | ---: | ---: | ---: |
| Rebuild | 24163.395 | 539.140 | 44.82 |
| Incremental update | 48.559 | 48.076 | 1.01 |

After the incremental phase there are 56 JSON files totaling 766,530 bytes.
All parsed documents match after removing index.rebuilt, using the numerical
tolerance above. Keys, strings and integer timestamps match exactly. The idle
SQLite samples range from 1.345 to 70.052 ms, so this run does not establish
an idle-time improvement. The [JSON report](v2-json-computed-128mib-cpu6.json)
includes all samples and observed thread affinity masks.

These results cover the initial four on-demand kernels. The expanded
implementation, including evapotranspiration, is measured below. A stored
derived column keeps its existing meaning and path. Arbitrary Python extension
code retains its Python implementation.
The native formulas scan bounded column blocks; their derived summaries are
not persisted. ARM runtime, SD-card behavior and cold-cache performance remain
unmeasured.

Reproduce after creating the ordinary fixture and installing the driver:

```sh
cp build/weather.sdb build/computed.sdb
python3 - build/computed.sdb <<'PY'
import math
import sqlite3
import sys

with sqlite3.connect(sys.argv[1]) as connection:
    connection.create_function('sin', 1, math.sin)
    connection.execute('ALTER TABLE archive ADD COLUMN outHumidity REAL')
    connection.execute('UPDATE archive SET outHumidity = '
                       'CASE WHEN ((dateTime-1704067200)/300)%41=0 THEN NULL '
                       'ELSE 60+35*sin(dateTime/18000.0) END')
PY
experimental/weewx-db/target/release/weewx-db import build/computed.sdb build/computed.wwdb
taskset -c 6 env PYTHONPATH=src python3 experimental/weewx-db/tools/bench_computed.py \
    build/computed.sdb build/computed.wwdb --rounds 3 --cache-mib 128 --workspace-mib 256
taskset -c 6 env PYTHONPATH=build/weewx-db-trial/json-source/src python3 \
    experimental/weewx-db/tools/bench_json.py build/computed.sdb build/computed.wwdb \
    --computed --rounds 3 --cache-mib 128 --workspace-mib 256 --source-ref 28ac6ab8
```

## Windowed XTypes, xaggs and astronomy

The expanded fixture copies computed.sdb and adds radiation and windDir. Its
1,051,200 records and original outTemp daily summaries remain. Radiation is
`MAX(0,700*sin((dateTime%86400-21600)/86400.0*2*pi))`; wind direction is
`ABS((dateTime/300)%360)`. No tested derived observation is stored.

Both drivers run on CPU 6 with the same affinity restriction as the earlier
measurements. Weather series produce hourly means; ET uses its configured
one-hour sliding input window. Historical xaggs produces 365 daily historical
means, each drawn from the matching calendar day across all stored years.
The reference extension is pinned as described in the
[backend reference](../reference.md). Every timed pair checks answers, units,
timestamps and NULLs. One warmup is excluded and three measured rounds
alternate driver order. Warm medians in ms:

| Workload | SQLite / Python or PyEphem | Rust backend | Baseline time / Rust time |
| --- | ---: | ---: | ---: |
| Annual hourly ET | 2238.569 | 108.397 | 20.65 |
| Annual hourly appTemp | 1151.587 | 50.897 | 22.63 |
| Seven-day hourly maxSolarRad | 60.388 | 17.261 | 3.50 |
| Annual daily historical_avg | 322.900 | 5.066 | 63.74 |
| Sun and Moon positions on 366 dates | 15.015 | 19.415 | 0.77 |

This table uses 128 MiB cache and 256 MiB native workspace. The
[4 MiB cache / 16 MiB workspace run](v2-all-xtypes-4mib-cpu6.json)
measures ET at 112.483 ms against 2246.862 ms, a factor of 19.98. Its other
results are similar. The [larger-budget report](v2-all-xtypes-128mib-cpu6.json)
contains every sample.

The astronomy-only comparison calls the standalone JSON API 732 times and
compares topocentric right ascension and declination. PyEphem already uses C
for its numerical work. The Rust API is slower in this workload, including
its per-call serialization and additional returned position fields. The
maxSolarRad series benefits from calculation inside the native database scan.
These measurements do not attribute the difference to storage alone.

The complete JSON report uses ET, appTemp and windchill, with the same
ten-year archive configuration and generator commit `28ac6ab8` as above.
It writes 56 JSON files totaling 687,599 bytes after the incremental phase.
All parsed outputs match within 1e-10 relative / 1e-9 absolute tolerance,
excluding index.rebuilt. Three-round medians in ms:

| Phase | SQLite / Python | Rust backend | Baseline time / Rust time |
| --- | ---: | ---: | ---: |
| Rebuild | 40959.801 | 3008.133 | 13.62 |
| Incremental update | 51.168 | 50.008 | 1.02 |
| No new data | 2.305 | 2.125 | 1.08 |

The rebuild SQLite samples range from 35.94 to 42.46 seconds. Incremental
and idle samples do not establish a useful improvement. The
[windowed JSON report](v2-json-windowed-128mib-cpu6.json) records all samples
and the observed thread affinity mask `[6]`.

Reproduce after the earlier fixture setup and fetching the pinned xaggs
reference:

```sh
cp build/weewx-db-trial/computed.sdb build/weewx-db-trial/complex.sdb
python3 - build/weewx-db-trial/complex.sdb <<'PY'
import math
import sqlite3
import sys
with sqlite3.connect(sys.argv[1]) as connection:
    connection.create_function('sin', 1, math.sin)
    connection.execute('ALTER TABLE archive ADD COLUMN radiation REAL')
    connection.execute('ALTER TABLE archive ADD COLUMN windDir REAL')
    connection.execute('UPDATE archive SET '
                       'radiation=MAX(0,700*sin((dateTime%86400-21600)/86400.0*6.283185307179586)),'
                       'windDir=ABS((dateTime/300)%360)')
PY
experimental/weewx-db/target/release/weewx-db import \
    build/weewx-db-trial/complex.sdb build/weewx-db-trial/complex.wwdb
taskset -c 6 env PYTHONPATH=src python3 experimental/weewx-db/tools/bench_xtypes.py \
    build/weewx-db-trial/complex.sdb build/weewx-db-trial/complex.wwdb --rounds 3
taskset -c 6 env PYTHONPATH=build/weewx-db-trial/json-source/src python3 \
    experimental/weewx-db/tools/bench_json.py \
    build/weewx-db-trial/complex.sdb build/weewx-db-trial/complex.wwdb \
    --observations ET,appTemp,windchill --cache-mib 128 --workspace-mib 256 \
    --rounds 3 --source-ref 28ac6ab8
```

The implementation also has independent PyEphem position, event and phase
comparisons, almanac checks with PyEphem calls disabled, short grazing-horizon
events, all standard XType/xaggs aggregates, provider-state checks and shared
percentile-budget checks. A deliberate mutation removing apparent solar
longitude from lunar phase searches failed the phase test by 56.999 seconds;
restoring it passed. The [mutation report](v2-astronomy-mutation.json) retains
the failure. Removing the custom-provider dispatch guard also made its
priority test fail; restoring it passed.

## Finding daily records

These queries search 3,650 stored daily summaries across the complete archive.
The wettest day maximizes the daily rain sum. The warmest mean day maximizes
`wsum/sumtime`; the temperature-peak day maximizes the stored daily `max`,
including imported LOOP extrema. Ties choose the earliest dateTime.

On CPU 6, warm medians of 101 queries through persistent Python connections
with a 128 MiB cache are:

| Query | SQLite ms | Rust backend ms |
| --- | ---: | ---: |
| Wettest day | 0.255 | 1.166 |
| Warmest mean day | 0.313 | 1.385 |
| Highest temperature day | 0.188 | 0.814 |

SQLite is currently faster for these small ORDER BY/LIMIT queries. They use
existing summaries and do not regroup every archive row. The original fixture
contains only outTemp daily summaries. Rain daily sums were added to temporary
copies of both databases before timing, using UTC archive-day boundaries.
That preparation is excluded from the query times. Results matched exactly
on every query. The SQL and samples are recorded in
[the CPU 6 report](v2-extremes-128mib-cpu6.json) and
[the unrestricted report](v2-extremes-128mib.json).

## Driver and engine queries

The Python comparison uses the same WeeWX source revision. With the 128 MiB
cache and 256 MiB workspace, its annual hourly series takes 14.079 ms with
Rust and 29.622 ms with SQLite. The raw month takes 1.111 and 2.791 ms.
The existing weighted daily mean takes 0.022 and 0.020 ms, respectively;
that small query has no useful gain.

The direct Rust API benchmark also records an archive-wide aggregate. Its
persisted summaries answer the full blocks without reading their payloads.
The resulting several-thousand-fold ratio against SQLite's raw scan
compares precomputed statistics with a scan. It does not represent normal
WeeWX reports, which can already read daily summaries.

Raw reports:
[Python, 4 MiB](v2-python-4mib.json),
[Python, 128 MiB](v2-python-128mib.json),
[engine, no column cache](v2-core-0mib.json),
[engine, 128 MiB](v2-core-128mib.json).

The direct benchmark uses bundled SQLite 3.53.2, cached statements, no mmap,
and one grouped SQLite query for hourly buckets. Both engines run in Rust.
Answers are checked after timing; count, min, max and NULLs match, with
floating-point sum tolerance of
`1e-9 + 1e-10 * max(abs(a), abs(b))`. It reports new-connection first queries
and medians after one warmup. Opening, importing and JSON serialization are
excluded from this microbenchmark.

The source main file occupies 177,381,376 bytes. The compressed snapshot
occupies 125,884,179 bytes, about 29% less. These sizes exclude sidecars.
Memory budgets cover cached columns or SQLite pages, not total process RSS.

## Writes

Writer throughput is secondary for station traffic. The Python reports
include 200 actual DaySummaryManager.addRecord calls per mode, with all daily
updates, reopen checks and a separate checkpoint time. Always mode
synchronizes every transaction before acknowledgement. Background mode
permits losing the newest transaction; the next writer waits for its
predecessor. Bursts therefore still pay synchronization costs. This is not
an unbounded, unsynchronized write benchmark.

Before the SQL-path changes, a 500-record profile attributed 622 ms to
COMMIT and 443 ms to other DB-API execute calls. A syscall trace measured
627 ms across 506 fsync calls. Repeated parsing and preparation were removed
from ordinary writes. The background mode addresses acknowledgement
latency at station intervals; it does not improve report generation.

## Reproduce

From the repository root, with the optional Python package and WeeWX test
dependencies installed:

```sh
cargo build --release --locked --manifest-path experimental/weewx-db/Cargo.toml
mkdir -p build/weewx-db-trial/json-source
git archive 28ac6ab8 src | tar -x -C build/weewx-db-trial/json-source
experimental/weewx-db/target/release/weewx-db fixture build/weather.sdb
experimental/weewx-db/target/release/weewx-db import build/weather.sdb build/weather.wwdb
PYTHONPATH=build/weewx-db-trial/json-source/src python3 \
    experimental/weewx-db/tools/bench_json.py build/weather.sdb build/weather.wwdb \
    --cache-mib 128 --workspace-mib 256 --rounds 7 --source-ref 28ac6ab8
PYTHONPATH=build/weewx-db-trial/json-source/src python3 \
    experimental/weewx-db/tools/bench_native.py build/weather.sdb build/weather.wwdb \
    --cache-mib 128 --workspace-mib 256 --rounds 7
```

Use a checkout containing the JSON generator through PYTHONPATH to compare
another source revision. The benchmark does not install or alter that source.

The earlier read-only, uncompressed prototype's reports remain available:
[no cache](desktop-0mib.json), [1 MiB](desktop-1mib.json),
[32 MiB](desktop-32mib.json). They are not measurements of the current writer
or driver.
