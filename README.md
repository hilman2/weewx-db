# WeeWX database

An optional Rust database for WeeWX 5. It replaces the SQLite archive of a
station with its own column store and runs the weather calculations, daily
aggregates and the solar/lunar almanac in Rust. It installs as one WeeWX
extension and needs no compiler, no pip and no PyEphem on the station. Your
SQLite archive stays on disk, and you can switch back at any time.

The backend is experimental. It has been measured on Linux x86_64 and on a
Raspberry Pi 4. Read the [supported operations and limits](reference.md) before
switching a station you rely on.

## What you gain

**Reports with calculated observations finish in a fraction of the time.**
Dewpoint, humidex, heat index, apparent temperature, wind chill and
evapotranspiration are calculated in Rust from the stored columns instead of
record by record in Python. On a ten-year archive with five-minute records, a
yearly hourly series of such an observation took 20 to 40 times less time than
WeeWX on SQLite, and a mean over the whole archive took 250 to 700 times less.
A JSON report rebuild that plots three calculated observations dropped from 24
seconds to half a second. The JSON files of WeeWX 5.6's Horizon skin, ten years
of stored observations, rebuilt six times as fast as on SQLite; on a
Raspberry Pi 4 in 33 instead of 220 seconds. Updating them after a new record
takes as long as on SQLite.

**Long-term statistics come from summaries, not scans.** Every stored block
carries count, sum, minimum and maximum. Historical aggregates such as the mean
of a calendar day across all years, the xaggs-style queries, ran 64 times
faster in the same measurements.

**Sun and Moon need no PyEphem.** Rise, set, transit, twilight, phase and
positions come from the Rust almanac, so the Sun and Moon details in the
Seasons almanac appear without installing PyEphem.

**Sensors are added in one statement.** `ALTER TABLE archive ADD COLUMN`
takes effect immediately. Old records read as NULL and nothing is rewritten.
No indexes are created or maintained by hand.

**You keep SQL.** The bundled CLI runs SQLite syntax with window functions,
time buckets and weather functions directly on the archive, and exports the
current data back to a SQLite file at any time.

**Memory stays within a budget you set.** Cache and workspace sizes are
options in `weewx.conf`. A Raspberry Pi trial can run with 4 and 16 MiB, a NUC
with 128 and 256 MiB. The budgets cap decoded columns and query workspace, not
the whole process.

**Classic skins render faster without changes.** Seasons, Belchertown,
Neowx-Material and Weather Data Center regenerate their pages after each
archive record 15 to 35 percent faster than on SQLite; on a Raspberry Pi 4,
Seasons took 6.3 instead of 9.5 seconds. Report runs reuse the
index, journal and statistics of earlier runs for periods without new data.
A complete first generation is 11 to 18 percent faster. Record queries such
as the wettest or warmest day answer two to eight times faster than SQLite.

The measurements behind these numbers, with raw reports and reproduction
commands, are in [results/README.md](results/README.md).

## Put it into operation

The walkthrough assumes a WeeWX 5 pip installation with its root in
`~/weewx-data`. On a package installation, the root is `/etc/weewx` and the
databases live in `/var/lib/weewx`; adjust the paths accordingly. Plan a few
minutes of downtime for the import. No archive record is lost: WeeWX catches
up from the station's logger after the restart if the driver supports it.

### 1. Install the extension

Download the ZIP for your machine from the
[releases](https://github.com/hilman2/weewx-db/releases) and install it with
`weectl`:

```sh
weectl extension install weewx-db-0.2.0-aarch64-unknown-linux-gnu.zip
```

The [extension guide](extension.md) lists the ZIP for each system, shows how to
verify the download and describes what the installer writes. In short, it
places the files under `bin/user/weewxdb` and adds a `weewx_rust` database
entry to `weewx.conf`. Nothing else changes yet; WeeWX keeps running on SQLite.

The extension does not include a JSON generator. JSON reports use the one that
ships with WeeWX 5.6 and read from the Rust database without changes.
Everything else works on any WeeWX 5.

### 2. Stop WeeWX and import the archive

Stop the daemon so the SQLite file does not change during the import:

```sh
sudo systemctl stop weewx
```

Import the archive into a new file next to it. The import reads a consistent
SQLite transaction, including committed WAL data, and copies the daily
summaries as they are, including LOOP extrema and time weights. The
destination must not exist yet. A ten-year archive with a million
five-minute records took about one second on a desktop and one minute on a
Raspberry Pi 4 throttled to 600 MHz.

```sh
~/weewx-data/bin/user/weewxdb/weewx-db import \
    ~/weewx-data/archive/weewx.sdb ~/weewx-data/archive/weewx.wwdb
~/weewx-data/bin/user/weewxdb/weewx-db info ~/weewx-data/archive/weewx.wwdb
```

`info` prints the schema, row count and timestamp bounds. Compare the row
count and the last timestamp with your SQLite archive, for example with
`sqlite3 weewx.sdb 'SELECT COUNT(*), MAX(dateTime) FROM archive'`. Use
`import --table NAME` if your archive table has another name.

### 3. Point the binding to the new database

Edit `weewx.conf`. The installer already added the database entry; you only
change the binding. Keep the manager, schema and `table_name` settings in the
binding as they are.

```ini
[Databases]
    [[weewx_rust]]
        database_type = SQLite
        driver = user.weewxdb.driver
        database_name = weewx.wwdb
        cache_mib = 32
        workspace_mib = 32
        sync_mode = background

[DataBindings]
    [[wx_binding]]
        database = weewx_rust
```

`database_type = SQLite` places the file in the directory that
`[DatabaseTypes]` names for SQLite, next to your SQLite archive on pip and
package installations alike. Skins such as Belchertown also read it to choose
their SQL syntax; the explicit `driver` still selects this database. Leave the
memory budgets at their defaults for the first run; the
[memory options](reference.md#memory-and-synchronization) describe what each
one covers.

With `sync_mode = background`, a commit is acknowledged after its journal
entry is written and synchronized by a worker. After a power failure at most
the newest archive record can be lost. Set `always` if you prefer each record
to be synchronized before WeeWX continues.

### 4. Start WeeWX and check the reports

```sh
sudo systemctl start weewx
```

Watch the log until the first archive interval has passed. The driver opens
the new file, registers its XTypes and almanac, and WeeWX writes the next
record into it. Then open the reports: the current conditions, the daily
plots and the almanac section should look as before. Yearly plots of
calculated observations are the ones that got faster.

To confirm that records arrive, query the newest one:

```sh
~/weewx-data/bin/user/weewxdb/weewx-db sql ~/weewx-data/archive/weewx.wwdb \
    "SELECT datetime(dateTime, 'unixepoch', 'localtime'), outTemp
     FROM archive ORDER BY dateTime DESC LIMIT 3"
```

Other bindings, for example a forecast or an air-quality database, keep using
SQLite. Only the bindings you point at `weewx_rust` move.

### 5. Enable the optional adapters

Two more pieces are off by default because they replace WeeWX classes:

- **Stored or calculated values in JSON reports on WeeWX 5.6.** Swap the
  generator class in the report's `skin.conf` to choose per line whether a
  value is read from its column or calculated. It is not needed for speed.
- **Packet calculations.** Swap `StdWXCalculate` in `weewx.conf` so the
  values in LOOP packets are calculated by the same Rust formulas.

The [extension guide](extension.md#choose-stored-or-calculated-values-in-json-reports) has the exact
lines to change for both.

### 6. If you want to go back

Stop WeeWX, point `wx_binding` back to the SQLite entry and start it again.
Your SQLite file is still there, but it lacks the records written since the
switch. To carry them over, export the current data to a new SQLite file and
use that one instead:

```sh
~/weewx-data/bin/user/weewxdb/weewx-db export \
    ~/weewx-data/archive/weewx.wwdb ~/weewx-data/archive/weewx-restored.sdb
```

`weectl extension uninstall weewx-db` removes the files; see
[removing the extension](extension.md#remove-the-extension).

## Keep it running

The database writes changes to a journal and folds them into the main file
when the journal reaches 8 MiB, so no scheduled maintenance is needed. These
commands help when you want to look:

```sh
weewx-db check weewx.wwdb              # verify checksums and structure
weewx-db info weewx.wwdb               # schema, row count, bounds, cache use
weewx-db checkpoint weewx.wwdb --optimize   # fold the journal now
weewx-db vacuum weewx.wwdb             # reclaim space after deletes
```

`weewx-db` stands for `bin/user/weewxdb/weewx-db` under your WeeWX root. For
backups, either export to SQLite as above or stop WeeWX and copy the `.wwdb`
file together with its `.wal`, `.head` and `.lock` sidecars. The
[storage explanation](architecture.md) describes the journal, checkpoints and
backup rules in detail.

## Query the database

A new sensor needs one schema operation. Historical records read as NULL
without rewriting the old blocks.

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
calculations are available as SQL functions, here on Celsius archive values:

```sh
weewx-db sql weewx.wwdb \
    'SELECT time_bucket(dateTime,3600) AS hour,
       AVG(humidex_c(outTemp,outHumidity)), MAX(humidex_c(outTemp,outHumidity))
     FROM archive WHERE dateTime > ? AND dateTime <= ?
     GROUP BY hour ORDER BY hour' \
    --param 1704067200 --param 1735689600 --format csv
```

The [SQL reference](reference.md#sql-and-calculations) lists native statistics,
window functions, the supported observations, Fahrenheit functions and XType
dispatch rules.

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

## Feed data without WeeWX

`init` creates a small default archive, and `append` adds JSONL records,
creating numeric sensors on request. A batch commits atomically; earlier
completed batches survive an input error.

```sh
weewx-db init readings.wwdb
weewx-db append readings.wwdb readings.jsonl --auto-add
weewx-db sql readings.wwdb \
    'SELECT dateTime,outTemp FROM archive ORDER BY dateTime' --format jsonl |
    jq -c 'select(.outTemp != null)'
```

For bulk input, one `append` process avoids opening a connection for each
record. For many queries, `batch` reads JSONL requests such as
`{"sql":"SELECT COUNT(*) FROM archive","params":[]}` in one session.
CSV, TSV and JSONL exports stream rows.

## Develop and test

Development uses a local Rust build and the `weewxdb` Python package in
`python/`. In this layout the driver is `weewxdb.driver` and the CLI is
`target/release/weewx-db`. Station installations use the extension instead.

Tests use pinned public reference sources listed in
[references.json](references.json): WeeWX, the WeeWX branch carrying the 5.6
JSON generator, xaggs and PyEphem. Python 3.13 and Rust 1.99 were used for the
recorded checks. Install a Rust toolchain, cargo-nextest, just, Python and Git,
then run from a fresh clone:

```sh
just setup    # virtual environment, pinned references, first build
just fast     # lint, build, Rust and Python tests against the pinned WeeWX
```

`just full` adds the Python tests against the branch with the WeeWX 5.6 JSON
generator. `just build` compiles the CLI and the Python module together, and
the test suite stops when either is older than the Rust sources. To use the
package outside the test runs, install it with `python -m pip install -e .`.
The reference fetcher verifies existing checkouts before reusing them.
`weewx-db fixture FILE.sdb` generates a synthetic SQLite archive, `import`
converts it and `bench` compares both. The repository contains the measurement
reports and reproduction tools; see [results/README.md](results/README.md).

`python3 tools/build_extension.py` packages a local build as an extension ZIP.
The [extension guide](extension.md#build-from-source) covers portable builds
and the GitHub Actions release matrix.
