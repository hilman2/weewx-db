# WeeWX database

This optional Rust backend stores WeeWX archive records and daily summaries
in its own format. It provides a weedb driver, a Python DB-API connection,
SQL queries and a pipeline CLI. Time-series calculations run in Rust through
an XType provider. The existing WeeWX installation stays on SQLite until you
select the new driver.

This repository preserves the database experiment originally developed inside
the WeeWX checkout. [Source provenance and licenses](NOTICE.md) identify the
upstream references and the original snapshot.

The backend is experimental. Read the [supported operations and limits](reference.md)
before switching an installation. [Recorded measurements](results/README.md)
include SQLite comparisons through the drivers and the JSON generator.

## Install as a WeeWX extension

The [extension guide](extension.md) covers building and installing the complete
package, SQLite migration, and enabling the JSON and packet adapters. It bundles
the Rust module and SQL CLI for the target platform. The JSON adapter uses the
generator supplied by WeeWX 5.6. [Batch API reference](batch.md) documents joint
queries, source policies and the bounded report cache.

## Build and try it

Run these commands from this directory. Use the Python interpreter that runs
your WeeWX installation when installing the optional driver.

```sh
cargo build --release --locked
python3 -m pip install .
mkdir -p build/weewx-db-trial
target/release/weewx-db fixture build/weewx-db-trial/weather.sdb
target/release/weewx-db import build/weewx-db-trial/weather.sdb \
    build/weewx-db-trial/weather.wwdb
target/release/weewx-db bench build/weewx-db-trial/weather.sdb \
    build/weewx-db-trial/weather.wwdb --cache-mib 32 --rounds 9
```

To test an existing archive, import its SQLite file instead of generating
the fixture. Import reads a consistent SQLite transaction, including committed
WAL data. The destination must not exist. Use `import --table NAME` for
another archive table name.

## Use it with WeeWX

After importing, add a database entry and point the existing data binding
to it. Keep the existing manager and schema settings in the binding.

```ini
[Databases]
    [[archive_rust]]
        driver = weewxdb.driver
        database_name = weather.wwdb
        SQLITE_ROOT = /var/lib/weewx
        cache_mib = 32
        workspace_mib = 32
        sync_mode = background

[DataBindings]
    [[wx_binding]]
        database = archive_rust
```

For the final switch, stop the archive writer, import its latest SQLite state
into a new destination, change the binding, then start WeeWX again. Preserve
the source database so you can compare reports. The driver registers its
XType provider when opened; no separate service entry is needed.
It also registers the [solar/lunar almanac](astronomy.md). Sun, Moon and
twilight calculations work without installing PyEphem.

The same backend runs with different RAM budgets. A small-device trial can
use `cache_mib = 4` and `workspace_mib = 16`. A NUC trial can use 128 and 256.
These are starting points for measurement. Neither setting limits total
process RSS. See the [memory options](reference.md#memory-and-synchronization).

To return the current data, including journaled changes, to SQLite:

```sh
target/release/weewx-db export /var/lib/weewx/weather.wwdb restored.sdb
```

## Add sensors and query data

A nullable sensor needs one schema operation. Historical records receive
NULL without rewriting the old payloads.

```sh
target/release/weewx-db sql weather.wwdb \
    'ALTER TABLE archive ADD COLUMN pm25 REAL'
target/release/weewx-db sql weather.wwdb \
    'SELECT time_bucket(dateTime,3600) AS hour, AVG(outTemp), MAX(pm25)
     FROM archive WHERE dateTime > ? AND dateTime <= ?
     GROUP BY hour ORDER BY hour' \
    --param 1704067200 --param 1735689600 --format csv
target/release/weewx-db sql weather.wwdb \
    'SELECT dateTime, outTemp,
       AVG(outTemp) OVER (ORDER BY dateTime ROWS BETWEEN 11 PRECEDING AND CURRENT ROW)
     FROM archive WHERE dateTime > 1704067200' --format jsonl
```

Use `--file query.sql` or stdin for longer SQL. Multiple statements execute
in order; use an explicit BEGIN and COMMIT when they must be atomic.
Individual Python `execute()` calls accept one statement and positional
parameters.

```python
import weewxdb

db = weewxdb.connect("weather.wwdb", cache_mib=128, workspace_mib=256)
with db.execute("SELECT AVG(outTemp) FROM archive WHERE dateTime > ?", (1704067200,)) as cursor:
    print(cursor.fetchone())
db.close()
```

The [SQL reference](reference.md#sql-and-calculations) lists native statistics,
window functions and weather calculations. No manual CREATE INDEX is needed.

For on-demand weather calculations, request the observation through WeeWX
as usual, or use a weather function in SQL:

```sh
target/release/weewx-db sql weather.wwdb \
    'SELECT time_bucket(dateTime,3600) AS hour,
       AVG(humidex_c(outTemp,outHumidity)), MAX(humidex_c(outTemp,outHumidity))
     FROM archive WHERE dateTime > ? AND dateTime <= ?
     GROUP BY hour ORDER BY hour' \
    --param 1704067200 --param 1735689600 --format csv
```

This example uses Celsius archive temperatures. See the reference for the
supported observations, Fahrenheit functions and XType dispatch rules.

## Use pipelines

`init` creates a small default archive. JSONL append can add numeric sensors
on request. A batch commits atomically; earlier completed batches survive an
input error.

```sh
target/release/weewx-db init weather.wwdb
target/release/weewx-db append weather.wwdb readings.jsonl --auto-add
cat readings.jsonl | xargs -d '\n' -I '{}' \
    target/release/weewx-db append weather.wwdb --row '{}'
target/release/weewx-db sql weather.wwdb \
    'SELECT dateTime,outTemp FROM archive ORDER BY dateTime' --format jsonl |
    jq -c 'select(.outTemp != null)'
```

For bulk input, one `append` process avoids opening a connection for each
record. For many queries, `batch` reads JSONL requests such as
`{"sql":"SELECT COUNT(*) FROM archive","params":[]}` in one session.
CSV, TSV and JSONL exports stream rows.

## Check and maintain the database

```sh
target/release/weewx-db check weather.wwdb
target/release/weewx-db info weather.wwdb
target/release/weewx-db checkpoint weather.wwdb --optimize
target/release/weewx-db vacuum weather.wwdb
```

Checkpoint updates affected blocks and their summaries. Vacuum reclaims
obsolete blocks. The [storage explanation](architecture.md) describes the
journal, automatic indices, synchronization and backup files.

## Run the development checks

Tests use pinned public WeeWX, JSON-generator, xaggs and PyEphem reference
sources listed in [references.json](references.json). Python 3.13 and Rust 1.99
were used for the recorded checks. Install a Rust toolchain, Python and Git,
then run from a fresh clone:

```sh
python3 -m venv .venv
. .venv/bin/activate
python -m pip install -r requirements-test.txt
python -m pip install -e .
python tools/fetch_references.py
cargo test --locked --tests
cargo clippy --all-targets --features python --locked -- -D warnings
cargo build --release --locked
PYTHONPATH=build/weewx-db-trial/weewx-source/src python -m pytest -q tests
PYTHONPATH=build/weewx-db-trial/json-source/src python -m pytest -q tests
```

The reference fetcher verifies existing checkouts before reusing them. Database
fixtures are synthetic and generated locally; the repository contains the
measurement reports and reproduction tools. See [results/README.md](results/README.md)
to reproduce the performance comparisons.
