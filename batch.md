# Batch API reference

The APIs live in `weewxdb.batch` for a Python installation and
`user.weewxdb.batch` for a WeeWX extension installation.

## Series

`get_series_many(requests, manager, cache=None, statistics=None)` returns a list
of WeeWX series tuples in request order. Each result contains the start vector,
stop vector and data vector, including their units.

Each request is a dictionary:

```python
from weeutil.weeutil import TimeSpan
from user.weewxdb.batch import get_series_many

series = get_series_many([
    {'observation': 'appTemp', 'span': TimeSpan(start, stop),
     'aggregate': kind, 'interval': 3600,
     'options': {'source': 'derived'}}
    for kind in ['avg', 'min', 'max']
], manager)
```

`observation` and `span` are required. `aggregate` and `interval` default to
`None`, which requests archive records without aggregation. `options` defaults
to an empty dictionary and accepts the usual XType options.

The engine fuses archive queries with identical ranges and unit systems.
Different observations share one cursor. Basic aggregates of an identical
calculated observation share the formula evaluation and reduction state.
Daily summaries, vectors, historical xaggs and percentiles retain their
specialized calculation paths. Custom XTypes keep WeeWX dispatcher priority.
The engine splits observations into smaller native groups when the workspace
budget is tight. Oversized result sets use individual queries.

The optional `statistics` dictionary receives `scans` and
`formula_evaluations` for the native batch. Formula evaluations count the fused
archive path; specialized queries have separate internal work.

## Aggregates

`get_aggregates_many(observation, span, aggregates, manager, **options)` returns
one WeeWX ValueTuple per requested aggregate. It follows WeeWX's normal choice
between daily summaries and archive records. For example:

```python
from user.weewxdb.batch import get_aggregates_many

average, minimum, maximum = get_aggregates_many(
    'appTemp', TimeSpan(start, stop), ['avg', 'min', 'max'], manager,
    source='derived')
```

## Source policy

The `source` option defaults to `auto`.

| Value | Data source |
| --- | --- |
| `auto` | Ordinary WeeWX selection, including stored columns and daily summaries. |
| `stored` | Stored archive column, preserving NULLs. |
| `derived` | Calculate every archive value using the registered standard XType settings. |
| `fill_missing` | Keep stored non-NULL values, calculate missing values. |

Explicit policies support raw archive series and these aggregates: `avg`,
`min`, `max`, `sum`, `count`, `not_null`, `mintime`, `maxtime`, `first`, `last`,
`firsttime`, `lasttime`, `cumulative`, `weighted_avg`, `integral`.
Unsupported explicit policies raise an exception. They do not select another
source. Derived ET and pressure values convert into the archive unit system
before mixing with stored values. Stateful formulas see the complete selected
series, including rows whose stored values are retained.

Explicit policies calculate from archive records. `auto` retains daily LOOP
extrema, weighted daily means and calendar boundaries. A derived archive minimum
can therefore differ from a daily minimum that includes LOOP packets.

## Report cache

`ReportCache(max_bytes=8 * 1024 * 1024)` stores bounded result payloads for one
report instance. The cache tracks the native snapshot identifier and journal
revision. Committed changes through other connections and checkpoints invalidate
cached results. Explicit transactions disable caching. Query settings form part
of the key; custom XType dispatch is checked before each lookup. Rain rate,
delta and pressure queries bypass this cache because they carry mutable provider
state. Returned vectors are independent copies.

The byte budget counts retained keys and result objects. It does not limit
Python's allocator, the complete report, or process RSS.

## WeeWX 5.6 JSON integration

WeeWX's own `weewx.jsongenerator.JSONGenerator` reads through the driver's
XType and needs no change. `user.weewxdb.json.JSONGenerator` subclasses it for
two additions:

- A line with an explicit `source` policy requests its series natively, with
  the policies above. Lines without one take the base generator's path. The
  empty-sensor check of every line uses the line's policy.
- Day/night files use the Rust solar ephemeris. Crossings use the upper solar
  limb and atmospheric refraction; civil twilight uses the solar centre at -6
  degrees without refraction. These event times differ from WeeWX's approximate
  `weeutil.Sun` calculations. The events of the current year are calculated
  once per process and reused by later reports.

The base generator retains file naming, conversion, incremental resume points
and output serialization. The checked reference generator is pinned in
`references.json`. The adapter checks the signature of the generator's
`_aggregate_line` method. If it changes, the adapter logs a message and leaves
every line, and the day/night files, to the base generator.
