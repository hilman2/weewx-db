# WeeWX database

An optional Rust database for WeeWX 5. It stores archive records and daily
summaries in its own format and ships as a complete WeeWX extension: weedb
driver, SQL CLI, native XTypes and a solar/lunar almanac. Your WeeWX
installation stays on SQLite until you select the new driver.

The backend is experimental. Read the [supported operations and limits](reference.md)
before switching a station. [Recorded measurements](results/README.md) compare
it with SQLite through the drivers and through the WeeWX 5.6 JSON generator.

## Install the extension

Download the ZIP for your machine from the
[releases](https://github.com/hilman2/weewx-db/releases) and install it with
`weectl`. Installation needs neither a Rust compiler nor pip:

```sh
weectl extension install weewx-db-0.2.0-aarch64-unknown-linux-gnu.zip
```

The [extension guide](extension.md) explains which ZIP to choose, how to verify
it, how to import your SQLite archive and switch the binding, and how to enable
the optional adapters.

The extension does not include a JSON generator. The bundled adapter extends the
generator that ships with WeeWX 5.6 and only works there. The database, XTypes
and almanac do not depend on it.

## What changes after the switch

The extension adds a `weewx_rust` database entry to `weewx.conf`. Once
`wx_binding` points to it, WeeWX writes archive records to the new file and the
reports read from it. The driver registers its XType provider and the
[solar/lunar almanac](astronomy.md) when opened. Sun, Moon and twilight
calculations work without PyEphem.

The database runs with different RAM budgets. A small-device trial can use
`cache_mib = 4` and `workspace_mib = 16`. A NUC trial can use 128 and 256.
Neither setting limits total process RSS. See the
[memory options](reference.md#memory-and-synchronization).

## Query the database

The extension installs the CLI as `bin/user/weewxdb/weewx-db` under your WeeWX
root, for example `~/weewx-data/bin/user/weewxdb/weewx-db` for a pip
installation or `/etc/weewx/bin/user/weewxdb/weewx-db` for a package
installation. The examples below call it `weewx-db`.

A nullable sensor needs one schema operation. Historical records receive NULL
without rewriting the old payloads.

```sh
weewx-db sql weewx.wwdb 'ALTER TABLE archive ADD COLUMN pm25 REAL'
weewx-db sql weewx.wwdb \
    'SELECT time_bucket(dateTime,3600) AS hour, AVG(outTemp), MAX(pm25)
     FROM archive WHERE dateTime > ? AND dateTime <= ?
     GROUP BY hour ORDER BY hour' \
    --param 1704067200 --param 1735689600 --format csv
weewx-db sql weewx.wwdb \
    'SELECT dateTime, outTemp,
       AVG(outTemp) OVER (ORDER BY dateTime ROWS BETWEEN 11 PRECEDING AND CURRENT ROW)
     FROM archive WHERE dateTime > 1704067200' --format jsonl
```

Use `--file query.sql` or stdin for longer SQL. Multiple statements execute in
order; use an explicit BEGIN and COMMIT when they must be atomic. Weather
calculations are available as SQL functions:

```sh
weewx-db sql weewx.wwdb \
    'SELECT time_bucket(dateTime,3600) AS hour,
       AVG(humidex_c(outTemp,outHumidity)), MAX(humidex_c(outTemp,outHumidity))
     FROM archive WHERE dateTime > ? AND dateTime <= ?
     GROUP BY hour ORDER BY hour' \
    --param 1704067200 --param 1735689600 --format csv
```

This example uses Celsius archive temperatures. The
[SQL reference](reference.md#sql-and-calculations) lists native statistics,
window functions, the supported observations, Fahrenheit functions and XType
dispatch rules. No manual CREATE INDEX is needed.

From Python, the extension package offers a DB-API connection. Each
`execute()` call accepts one statement and positional parameters:

```python
from user import weewxdb

db = weewxdb.connect("weewx.wwdb", cache_mib=128, workspace_mib=256)
with db.execute("SELECT AVG(outTemp) FROM archive WHERE dateTime > ?", (1704067200,)) as cursor:
    print(cursor.fetchone())
db.close()
```

The [batch API reference](batch.md) documents joint series and aggregate
queries, source policies and the bounded report cache.

## Feed and export data

`init` creates a small default archive. JSONL append can add numeric sensors
on request. A batch commits atomically; earlier completed batches survive an
input error.

```sh
weewx-db init readings.wwdb
weewx-db append readings.wwdb readings.jsonl --auto-add
cat readings.jsonl | xargs -d '\n' -I '{}' weewx-db append readings.wwdb --row '{}'
weewx-db sql readings.wwdb \
    'SELECT dateTime,outTemp FROM archive ORDER BY dateTime' --format jsonl |
    jq -c 'select(.outTemp != null)'
```

For bulk input, one `append` process avoids opening a connection for each
record. For many queries, `batch` reads JSONL requests such as
`{"sql":"SELECT COUNT(*) FROM archive","params":[]}` in one session.
CSV, TSV and JSONL exports stream rows.

To return the current data, including journaled changes, to SQLite:

```sh
weewx-db export weewx.wwdb restored.sdb
```

## Check and maintain the database

```sh
weewx-db check weewx.wwdb
weewx-db info weewx.wwdb
weewx-db checkpoint weewx.wwdb --optimize
weewx-db vacuum weewx.wwdb
```

Checkpoint updates affected blocks and their summaries. Vacuum reclaims
obsolete blocks. The [storage explanation](architecture.md) describes the
journal, automatic indices, synchronization and backup files.

## Develop and test

Development uses a local Rust build and the `weewxdb` Python package installed
with pip. In this layout the driver is `weewxdb.driver` and the CLI is
`target/release/weewx-db`. Station installations use the extension instead.

Tests use pinned public reference sources listed in
[references.json](references.json): WeeWX, the WeeWX branch carrying the 5.6
JSON generator, xaggs and PyEphem. Python 3.13 and Rust 1.99 were used for the
recorded checks. Install a Rust toolchain, Python and Git, then run from a
fresh clone:

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

The reference fetcher verifies existing checkouts before reusing them.
`weewx-db fixture FILE.sdb` generates a synthetic SQLite archive, `import`
converts it and `bench` compares both. The repository contains the measurement
reports and reproduction tools; see [results/README.md](results/README.md).

`python3 tools/build_extension.py` packages a local build as an extension ZIP.
The [extension guide](extension.md#build-from-source) covers portable builds
and the GitHub Actions release matrix.
