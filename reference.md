# Backend reference

## Schema and migration

Numeric tables use a unique INTEGER `dateTime` and INTEGER or REAL sensor
columns. Values follow SQLite's numeric affinity: numeric strings become
numbers, integral floating-point values in INTEGER columns become integers,
and other REAL values stay REAL in an INTEGER column. INTEGER storage
preserves 64-bit values; REAL storage preserves the bits, including infinity
and negative zero; NaN is stored as NULL. Text that is not a number fails with
"datatype mismatch", which WeeWX treats like a duplicate record. The dateTime
key, NOT NULL and numeric defaults are enforced; an INSERT that omits a NOT
NULL column with a default stores the default.

CREATE TABLE, DROP TABLE and ALTER TABLE with ADD COLUMN, RENAME COLUMN,
DROP COLUMN and RENAME TO are supported, one operation per statement, so
`weectl database add-column`, `rename-column` and `drop-columns` work. New
columns use NULL or their default for older records. A new NOT NULL column
needs a non-NULL default. Defaults are evaluated when the schema is
introduced. dateTime cannot be renamed or dropped. Dynamic defaults,
additional unique keys, CHECK/foreign-key constraints, triggers and views are
outside the supported schema.
Small tables without dateTime are metadata tables, limited to 4,096 rows.

Import includes the selected archive, its numeric `*_day_*` tables and its
`*_day__metadata` table. Columns declared NUMERIC, DECIMAL, BOOLEAN or without
a type are stored as REAL; text columns stop the import. It preserves stored daily values, including LOOP
extrema and time weights. Unrelated tables and user indices are not imported.
Export includes committed changes that are not yet compacted, and the current
schema definitions.

## SQL and calculations

The SQL frontend uses bundled SQLite over Rust virtual tables. Storage and
data access use Rust. Queries can use joins, CTEs, date functions, arithmetic,
filters, ORDER BY, GROUP BY, LIMIT and SQLite window syntax. INSERT, UPDATE,
DELETE, INSERT OR IGNORE and INSERT OR REPLACE are supported. SQLite UPSERT
ON CONFLICT and SQL savepoints are not supported for the archive tables.

Simple COUNT, SUM, AVG, MIN and MAX queries with dateTime bounds read persisted
Rust summaries where possible. The same functions grouped by
`time_bucket(dateTime, seconds)` use native interval calculations. Unsupported
query shapes use the SQL frontend and Rust column cursors. SUM and AVG also
have Rust window implementations. INTEGER sums check the final 64-bit result;
intermediate-overflow behavior can differ from SQLite. Numerical sums use
compensated arithmetic and can differ in their last bits from SQLite.

Additional Rust SQL functions:

| Function | Result |
| --- | --- |
| `time_bucket(timestamp, seconds)` | Start of a fixed UTC bucket, rounded toward negative infinity |
| `stddev_pop(value)`, `stddev_samp(value)` | Population or sample standard deviation |
| `variance_pop(value)`, `variance_samp(value)` | Population or sample variance |
| `percentile_cont(value, fraction)` | Exact interpolated percentile; fraction is constant in each group |
| `wind_vec_avg(speed, direction, weight)` | Magnitude of the weighted mean wind vector |
| `wind_vec_dir(speed, direction, weight)` | Direction in degrees; NULL for an exactly zero vector |
| `dewpoint_c(temperature, humidity)`, `dewpoint_f(temperature, humidity)` | Dewpoint; temperature and result in Celsius or Fahrenheit, humidity in percent |
| `humidex_c(temperature, humidity)`, `humidex_f(temperature, humidity)` | Humidex in the selected temperature unit |
| `heatindex_c(temperature, humidity)`, `heatindex_f(temperature, humidity)` | WeeWX's new heat-index algorithm in the selected temperature unit |
| `heatindex_old_c(temperature, humidity)`, `heatindex_old_f(temperature, humidity)` | WeeWX's old heat-index algorithm in the selected temperature unit |
| `wx_value(formula, timestamp, usUnits, parameters_json, inputs...)` | Stateless weather formula using the units of the input record |
| `astro_position(body, timestamp, latitude, longitude, elevation)` | JSON solar/lunar position; angles in degrees, elevation in meters |
| `astro_events(body, start, stop, latitude, longitude, elevation, horizon, center)` | JSON rise/set/transit events; horizon in degrees, center is 0 or 1 |

The XType provider handles raw numeric series and sum, average, min, max,
count and cumulative series. WeeWX calculates calendar boundaries, including
DST and month spans; Rust scans the measurements. Eligible daily queries use
WeeWX's stored daily summaries so LOOP extrema and weighted means survive.

The provider implements the standard WeeWX calculation classes:

| WeeWX provider | Native calculations |
| --- | --- |
| WXXTypes | dewpoint, inDewpoint, humidex, both heat-index algorithms, windchill, appTemp, cloudbase, beaufort, windrun, maxSolarRad with RS or Bras, and calm-wind direction handling |
| ETXType | Evapotranspiration with the configured period, station altitude/location, wind height, albedo, cn and cd |
| PressureCooker | pressure using the cached nearest 12-hour temperature, barometer, and altimeter algorithms aaASOS, aaASOS2, aaMADIS, aaNOAA, aaWOB and aaSMT |
| RainRater | Rain rate from the provider's LOOP queue and its initial archive augmentation |
| Delta | Configured differences of cumulative counters, including resets and missing values |
| ArchiveTable / DailySummaries | Standard scalar aggregates, weighted daily averages, daily extrema/times and threshold counts |
| WindVec / WindVecDaily | Raw and aggregated wind vectors, including zero-speed and missing-direction rules |
| AggregateHeatCool | heatdeg, cooldeg and growdeg with configured temperature bases |

On-demand calculations require the corresponding standard WeeWX scalar
provider to be registered. Stored columns and daily-summary observations
retain their meaning. Preceding custom providers and instance overrides
retain their Python dispatch. US, METRIC and METRICWX are supported.
Mixed archive units raise UnsupportedFeature for computed series; ET retains
its own missing-window and mixed-window behavior. Scalar calculations update
the existing PressureCooker, RainRater and Delta objects, so subsequent LOOP
processing sees the same state.

Raw, sum, average, min, max, count and cumulative weather series use one native
request. Exact archive, vector and daily-summary rules depend on the operation;
for example raw wind vectors include the lower bound while ordinary archive
series exclude it. Daily extrema include imported LOOP extrema. Custom
calendar boundaries still come from WeeWX.

All eleven [weewx-xaggs](https://github.com/tkeffer/weewx-xaggs) aggregates are
available: historical_min, historical_max, historical_min_avg,
historical_max_avg, historical_mintime, historical_maxtime, historical_avg,
avg_ge, avg_gt, avg_le and avg_lt. Historical queries require a single local
calendar day with the extension's midnight/archive boundary rules and search
that month/day across all stored years. Historical series share one scan.
Thresholds use the normal `val=(value, unit, group)` option. Reference-extension
compatibility is pinned to commit `d36145689b6d4bd363a792432df94215d69f5026`;
unrecognized customized providers keep their Python implementation.
The extension's copyright notice and LGPL terms accompany the native port in
`src/analytics.rs` and [vendor/LICENSE.xaggs](vendor/LICENSE.xaggs).

Additional native aggregate names are median, percentile, stddev, stddev_samp,
variance, rms, slope, weighted_avg, integral, maxgap, longest_ge and longest_lt.
`percentile` uses `fraction=0.5` by default, with linear interpolation over
non-NULL values. `slope` is ordinary least squares per second. `weighted_avg`
and `integral` use each archive record's interval in seconds. `maxgap` is the
largest gap between archive timestamps. Streaks count consecutive qualifying
rows, with NULL breaking a streak. On eligible daily spans they count days
whose weighted daily mean meets `val`; gaps longer than 36 hours break a
daily streak. These new statistics use archive readings unless a daily rule
is stated here.

Variance, slope, integral and temperature standard deviations return
unspecified WeeWX units to avoid invalid dimensional or offset conversions.
Their numeric values use stored input units, squared for variance and
multiplied by seconds for integral. Other new aggregates retain the observation
unit; maxgap returns seconds and streaks return counts. SQL queries can convert
inputs explicitly before reducing them.

SQL COUNT, SUM, AVG, MIN and MAX over these two-column weather functions
use native calculations with dateTime bounds, including time_bucket groups.
Each reading is calculated before aggregation. Queries with other expression
shapes use the same Rust functions through the SQL frontend. SQL SUM of an
empty set is NULL; WeeWX's computed-XType SUM follows XTypeTable and returns
zero. Missing inputs produce NULL calculated values.

`wx_value` accepts dewpoint, humidex, heatindex, heatindex_old, windchill,
appTemp, cloudbase, beaufort, windrun, barometer, altimeter, pressure,
solar_rs and solar_bras. Inputs follow the corresponding kernel in
`python/weewxdb/native.py`; pressure also needs the cached 12-hour temperature
in Fahrenheit as its last input. Parameters are a JSON object, with `{}`
selecting defaults. Windowed ET, rain rate and provider-state updates use the
XType API instead. For example:

```sql
SELECT time_bucket(dateTime, 3600),
       AVG(wx_value('appTemp', dateTime, usUnits, '{}', outTemp, outHumidity, windSpeed))
FROM archive GROUP BY 1;
```

The general SQL expression path invokes Rust per row. The XType series path
combines calculation and aggregation in its native scan. Arbitrary Python
formulas are not compiled into Rust. JSON formatting and report unit conversion
remain in the generator. The [astronomy reference](astronomy.md) describes the
solar/lunar API and WeeWX almanac integration.

`sqlite3` cannot open this custom file directly. Use the supplied SQL CLI or
DB-API connection, or export a SQLite file for tools that require that format.

## Memory and synchronization

| Driver / Python option | Default | Meaning |
| --- | --- | --- |
| `cache_mib` | 32 | Maximum cached, decompressed column bytes; connections to the same file in one process share the largest requested cache, and zero for all of them disables it |
| `workspace_mib` | 32 | Budget for native interval results, rolling windows, materialized SQL cells, fetch batches and exact percentiles |
| `result_cache` | 4096 | weedb driver only: aggregate results kept per database and generation, at most two generations; zero disables the cache |
| `timeout` | 5 | Maximum seconds a write waits for another writer |
| `sync_mode` | always | Commit durability: `always` or `background` |

Aggregate results let report runs reuse statistics of periods that received
no new data. Skins with many statistics pages, such as Neowx-Material, reuse
more results with `result_cache = 16384`, at about 20 MiB more memory.

SQL and batch CLI commands expose `--cache-mib` and `--workspace-mib`.
The workspace budget is per connection or operation, the cache budget per file
and process; neither is a total RSS cap. Indices, the
overlay of changed rows, SQLite's page cache of 2 MiB per connection, Python objects, concurrent aggregate groups, SQLite workspace
and the OS file cache use additional memory. SQLite temporary sorts may spill
to disk. XType exact percentiles share one sample budget across all output
bins and reserve 32 bytes per reading for capacity and sorting. SQL percentile
states have individual budgets; concurrent SQL groups can exceed one state's
limit in total. Large output should use streaming or narrower time spans.

`always` synchronizes each commit before it returns (SQLite
`synchronous=FULL` in WAL mode), so an acknowledged transaction survives a
process crash and a power failure. `background` (`synchronous=NORMAL`) skips
that synchronization; after a power failure the transactions since the last
WAL checkpoint can be lost, but the file stays consistent. Python
`connection.flush()` runs a WAL checkpoint.

One writer at a time holds SQLite's write lock; another writer waits up to
`timeout` seconds and then fails with "database is locked". Readers never wait
for a writer and see the last transaction committed before their statement.
Streaming read cursors keep their connection's snapshot until closed. Writes
on that connection can run while such a cursor is open; the automatic
compaction then waits for a later transaction. Schema changes need closed
cursors. A transaction is staged in memory until commit and is limited to
256 MiB. When a table has 1,024 changed rows, or all tables 16,384, the next
transaction first folds them into blocks.

Linux x86_64 is tested here. The code uses portable Rust, bundled SQLite and
portable LZ4, with no x86-specific instruction requirement. Build and measure
on Raspberry Pi before selecting budgets; ARM runtime and SD-card performance
have not been validated in this environment.
