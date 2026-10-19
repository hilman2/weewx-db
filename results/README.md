# Desktop measurements, 18 October 2026

The raw reports retain paths from the original WeeWX working tree. Commands
on this page use the standalone repository layout.

These measurements use a synthetic ten-year archive with 1,051,200
five-minute records, 32 observation columns and 16 active sensors. The host
is Linux x86_64 on an AMD Ryzen AI 9 HX 470 with NVMe storage, Rust 1.99.0
and Python 3.13.16. They are not Raspberry Pi measurements.

## Report skins, 19 October 2026

These runs generate complete reports with the four skins below, through
WeeWX's report engine, once on SQLite and once on the Rust driver. The archive
comes from `tools/station_fixture.py`: ten years of five-minute records in the
`wview_extended` schema, 1,051,200 records with daily summaries. Seasons comes
with the pinned WeeWX source; the other skins are pinned in
[references.json](../references.json). The host is the same as below, with
Python 3.14.

Each skin and engine runs in its own process on its own physical core. The
complete run writes into an empty output directory, including every monthly
and yearly summary page. The seven repeated runs use the same generation time,
as WeeWX does after every archive interval, and regenerate only the current
pages. Belchertown's forecast, AQI, earthquake and MQTT features are off.
Times in seconds; repeated runs are medians:

| Skin | Files | SQLite complete | Rust complete | SQLite repeated | Rust repeated |
| --- | ---: | ---: | ---: | ---: | ---: |
| Seasons | 193 | 2.961 | 2.423 | 0.472 | 0.326 |
| Belchertown | 190 | 7.217 | 5.986 | 0.406 | 0.378 |
| Neowx-Material | 594 | 10.671 | 9.369 | 1.174 | 1.030 |
| Weather Data Center | 275 | 172.996 | 153.688 | 3.880 | 3.340 |

Profiles of the repeated runs put most of the remaining time in template
rendering and in the skins' own Python code. Weather Data Center spends most
of its time in a table builder that searches its own rows for every value.
The [raw report](v3-skins.json) lists every sample.

The same measurement found two compatibility faults that these numbers
include as fixed. Templates that call `sun(use_center=1)` failed in the
almanac adapter, which only accepted a boolean. Belchertown reads
`database_type` from the database entry to choose its SQL dialect; the
extension's entry did not set it.

## Per-query costs, 19 October 2026

A query's fixed cost covers the check for changes by other connections, SQL
preparation and the Python DB-API. These medians are on CPU 6, as in the
sections below, through `weewxdb.connect`:

| Statement | Before | After |
| --- | ---: | ---: |
| `SELECT 1`, native call | 6.5 µs | 1.2 µs |
| `SELECT 1`, DB-API with fetch and close | 11.0 µs | 3.1 µs |
| `MAX(dateTime)` over the archive | 15.3 µs | 2.7 µs |
| One complete archive record by `dateTime` | 33.1 µs | 6.8 µs |

Reading a record no longer allocates a zeroed buffer for every all-NULL
column. A read checks one file for changes instead of opening the lock file
and checking three files. Prepared statements and their analysis are reused
for the same SQL text.

The daily-record queries of the [earlier comparison](#finding-daily-records)
now run on the virtual table's ordered paths. ORDER BY on a column with LIMIT
keeps the best rows while skipping blocks whose summaries cannot reach them.
ORDER BY on a computed expression, such as `wsum/sumtime`, is evaluated
natively with SQLite's arithmetic rules. Warm medians of 101 queries, ms:

| Query | SQLite | Rust backend |
| --- | ---: | ---: |
| Wettest day | 0.236 | 0.093 |
| Warmest mean day | 0.286 | 0.105 |
| Highest temperature day | 0.215 | 0.027 |

Repeated identical aggregates within one report, such as the same monthly
maximum in several templates, are answered from a per-connection cache for the
current database revision. Writes, other connections' commits and open
transactions bypass or reset it. A repeated `$month.outTemp.max` then takes
2.6 µs instead of 11.9 µs on SQLite.

The JSON generator's idle run, with no new data, takes 2.0 ms instead of
2.8 ms on SQLite with the 128 MiB cache. Its incremental update remains equal.
Sun and Moon positions match PyEphem's time within one percent. Fetching the
newest record or the first and last timestamps through WeeWX's manager stays
within one microsecond of SQLite.

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
    tools/bench_json.py build/weather.sdb build/weather.wwdb \
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
target/release/weewx-db import build/computed.sdb build/computed.wwdb
taskset -c 6 env PYTHONPATH=build/weewx-db-trial/weewx-source/src python3 tools/bench_computed.py \
    build/computed.sdb build/computed.wwdb --rounds 3 --cache-mib 128 --workspace-mib 256
taskset -c 6 env PYTHONPATH=build/weewx-db-trial/json-source/src python3 \
    tools/bench_json.py build/computed.sdb build/computed.wwdb \
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
target/release/weewx-db import \
    build/weewx-db-trial/complex.sdb build/weewx-db-trial/complex.wwdb
taskset -c 6 env PYTHONPATH=build/weewx-db-trial/weewx-source/src python3 tools/bench_xtypes.py \
    build/weewx-db-trial/complex.sdb build/weewx-db-trial/complex.wwdb --rounds 3
taskset -c 6 env PYTHONPATH=build/weewx-db-trial/json-source/src python3 \
    tools/bench_json.py \
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

## Batched XTypes and the WeeWX extension

The [extension](../extension.md) combines the database, driver and native
calculation providers in one platform archive. Its JSON adapter subclasses the
installed generator. The checked generator source is `28ac6ab8691b`, the pinned
reference for the WeeWX 5.6 integration. Final released 5.6 compatibility remains
to be checked against the shipped generator.

On the same CPU 6, a 365-day query asks for hourly ET, appTemp and windchill,
each as avg/min/max. The SQLite baseline excludes the native XType provider.
Both database caches are set to 32 MiB; Rust workspace is 32 MiB. Three timed
rounds per engine use a warm OS cache. All vectors, units, timestamps and NULLs
match WeeWX within the recorded float tolerance.

| Execution | Median ms |
| --- | ---: |
| SQLite and Python XTypes | 11509.007 |
| Rust, nine separate queries | 610.186 |
| Rust, one batch | 327.546 |

Batching adds a 1.86x gain over separate Rust requests and a
35.14x gain over the SQLite/Python baseline. One archive cursor serves
all observations. Each source formula runs once per record, for 315,360 formula
evaluations across the three observations. Low workspace budgets split sources
into smaller groups while retaining all reductions of each source together.
The [raw report](v2-batch-cpu6.json) records samples and affinity.

A complete JSON comparison adds min/max arrays to the same three-observation
report on the ten-year fixture. It produces 56 files and 1,970,269 bytes. This is
one timed run per phase, rather than a three-round median. Cache is 32 MiB and
workspace is 64 MiB. Day/night files are disabled in this throughput comparison.

| JSON phase | SQLite ms | Extension ms |
| --- | ---: | ---: |
| rebuild | 108705.580 | 5209.774 |
| incremental | 129.267 | 86.961 |
| idle | 1.764 | 1.830 |

The [JSON report](v2-json-batch-extremes-cpu6.json) records output equivalence
and configuration. Incremental generation reuses the base generator's resume
cache. Idle generation has no useful speed advantage.

The [mutation report](v2-batch-mutations.json) records failures after deliberately
breaking source policies, cache invalidation, formula sharing, all-NULL sensor
selection and resume caching. Restoring each change passes its test. The final
suite has 468 passing Python tests with the JSON reference, 462 with the base
reference plus one skipped JSON module, and 22 Rust tests. The extension tests
install, run and remove a complete archive in a temporary WeeWX station.
No Raspberry Pi timing was collected.

Reproduce with the complex fixture from the preceding section:

```sh
PYTHONPATH=build/weewx-db-trial/json-source/src taskset -c 6 .venv/bin/python \
    tools/bench_batch.py build/weewx-db-trial/complex.sdb \
    build/weewx-db-trial/complex.wwdb --rounds 3
PYTHONPATH=build/weewx-db-trial/json-source/src taskset -c 6 .venv/bin/python \
    tools/bench_json.py build/weewx-db-trial/complex.sdb \
    build/weewx-db-trial/complex.wwdb --rounds 1 --batch --extremes \
    --observations ET,appTemp,windchill --source-ref 28ac6ab8691b
```

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

SQLite was faster for these small ORDER BY/LIMIT queries in this measurement.
They use existing summaries and do not regroup every archive row. The
[per-query section](#per-query-costs-19-october-2026) shows the current
ordered paths. The original fixture
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

From this repository root, after the development setup in
[README.md](../README.md):

```sh
cargo build --release --locked
python3 tools/fetch_references.py
target/release/weewx-db fixture build/weather.sdb
target/release/weewx-db import build/weather.sdb build/weather.wwdb
PYTHONPATH=build/weewx-db-trial/json-source/src python3 \
    tools/bench_json.py build/weather.sdb build/weather.wwdb \
    --cache-mib 128 --workspace-mib 256 --rounds 7 --source-ref 28ac6ab8
PYTHONPATH=build/weewx-db-trial/json-source/src python3 \
    tools/bench_native.py build/weather.sdb build/weather.wwdb \
    --cache-mib 128 --workspace-mib 256 --rounds 7
```

Use a checkout containing the JSON generator through PYTHONPATH to compare
another source revision. The benchmark does not install or alter that source.

The skin runs need Cheetah3, and on Python 3.13 or later also legacy-cgi.
Run each skin and engine in its own process:

```sh
python3 -m pip install Cheetah3 legacy-cgi
PYTHONPATH=build/weewx-db-trial/weewx-source/src python3 \
    tools/station_fixture.py build/station.sdb
target/release/weewx-db import build/station.sdb build/station.wwdb
for skin in Seasons Belchertown neowx-material weewx-wdc; do
    for engine in sqlite rust; do
        PYTHONPATH=build/weewx-db-trial/weewx-source/src python3 tools/bench_skins.py \
            $engine $skin build/station.sdb build/station.wwdb --runs 7
    done
done
```

The earlier read-only, uncompressed prototype's reports remain available:
[no cache](desktop-0mib.json), [1 MiB](desktop-1mib.json),
[32 MiB](desktop-32mib.json). They are not measurements of the current writer
or driver.
