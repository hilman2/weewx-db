# Storage and query execution

For installation and queries, start with the [usage guide](README.md).
This page describes the implementation.

Archive blocks contain at most 4,096 rows, sorted by dateTime. Daily-summary
blocks contain at most 128 rows so changing today's summary rewrites a small
block. Columns hold a NULL bitmap and little-endian 64-bit values. Entirely
NULL columns need no payload. Each nonempty column uses LZ4 when that reduces
its size, and has a CRC over the stored bytes.

A database is one SQLite file in WAL mode. SQLite provides atomic commits,
crash recovery, locking between processes, reuse of freed pages and
consistent copies; the column format, the summaries and the query execution
belong to this project. `PRAGMA application_id` marks the file and
`PRAGMA user_version` holds the format version, currently 3. The file holds
these tables:

| Table | Content |
| --- | --- |
| `meta` | Database identity, archive table name, last transaction number, index generation, id counters |
| `tables` | Numeric tables by ordinal, with their CREATE TABLE text and column types; ordinals are never reused |
| `metadata` | Small tables without dateTime, such as `archive_day__metadata`, as whole row lists |
| `blocks` | One row per block: time range, row count and a descriptor per column |
| `chunks` | Column payloads by id; ids are never reused |
| `garbage` | Chunks replaced at an index generation, deleted one compaction later |
| `delta` | The current value of every row changed since the last compaction, NULL for a deletion, with its transaction number |

A column descriptor holds the chunk id, length, CRC, compression flag and the
column's summary: count, compensated sum, min and max. These summaries answer
complete unaffected blocks without decoding them. The time index selects
blocks and binary searches within them. Min/max and NULL counts also exclude
blocks for observation filters; SQL rechecks the remaining rows. Columns are
fetched only when projected. A bounded FIFO cache holds decoded columns;
connections to the same file share it. Schema additions synthesize missing old
column blocks from a default. INTEGER columns accept REAL values as SQLite
does; a block of such a column carries a second bitmap that marks its REAL
rows. REAL values may be infinite; NaN is stored as NULL.

A transaction is staged in memory and written at commit as one SQLite
transaction: its changed rows go to `delta`, together with schema changes and
metadata rows. A connection holds the changed rows in a sorted overlay.
Readers merge the overlay with the blocks and suppress replaced or deleted
readings before aggregation. Before each statement a connection asks SQLite
whether another connection committed, and then reads the newer rows of
`delta`. Readers never wait for a writer; a writer waits up to `timeout` for
another writer.

When a table has 1,024 changed rows, or all tables together 16,384, the next
write first folds them into blocks. Only blocks with changes are rewritten,
and rows after the last block fill it up to its capacity before a new block
starts. The new blocks, the updated index and the removal of the folded rows
from `delta` commit together. Chunks the compaction replaced become garbage of
the new index generation; the next compaction deletes them, so a reader in
another process that still holds the previous index can finish its query.
SQLite reuses the freed pages, so the file grows with the data, not with the
number of compactions. `weewx-db vacuum` deletes all garbage at once and
rebuilds the file without free pages.

WeeWX opens new connections for every report run. Connections to the same
file in one process therefore share what does not change between runs: the
block index per index generation, the decoded column cache and the changed
rows read so far. A new connection reads only the rows of `delta` written after
those, while the generation is the same. Only committed state is shared. The
column cache is keyed by chunk id and compares the descriptor's CRC on every
hit, so a chunk id that a rolled-back transaction had allocated, or another
file at the same path, cannot serve stale bytes. Each connection's own
uncommitted changes stay private, because shared rows and schemas are copied
on write.

A process-wide change log records which table and time range every published
transaction touched. Rows read from `delta` and own commits are recorded;
compactions record no data change. Consecutive transactions that touch
nearby times of one table merge into one entry, so a WeeWX station keeps about
one entry per table and day. The WeeWX XType adapter keeps aggregate results
per file and database identity, with the time ranges they read. A later
revision reuses a result when the log shows no change in those ranges since it
was computed. A transaction number missing from the log, for example one
another process folded into a compaction, invalidates the result. Results are
never used inside a transaction, and stateful weather providers bypass the
cache. Spans ending within two days of the newest record mostly move with the
report time; their results stay with one connection and revision. The other
results are kept per database in two generations of `result_cache` entries.

Files belonging to one database are the `.wwdb` file and, while it is open,
SQLite's `-wal` and `-shm` files. SQLite creates them with the permissions of
the database file and, when it runs as root, gives them the database file's
owner. `weewx-db backup` writes a consistent copy while WeeWX writes. For a
filesystem copy, stop WeeWX first and copy the `.wwdb` file with a `-wal`
file if one is left. The locks are POSIX advisory locks. Another SQLite
library in the same process, such as Python's `sqlite3` module, does not see
them, and closing any descriptor of the file releases them for the whole
process. Inside weewxd, open the database only through the driver.

Opening checks the application id, the format version and that the file
contains exactly the tables above, with no triggers or views. Stored table
definitions must be a single CREATE TABLE statement for that table before the
SQL frontend or an export executes them. `weewx-db check` runs SQLite's
integrity check and verifies every chunk's length and CRC, the presence, order
and bounds of timestamps, and the stored summaries.

The SQL frontend keeps schema and small metadata tables in memory. Numeric
virtual tables access Rust blocks. Common scalar and time-bucket aggregates
bypass row-by-row SQL execution; remaining expressions use SQLite's planner
and Rust cursors. The optional PyO3 module releases the GIL during engine work.
WeeWX's XType adapter sends numeric time ranges to Rust in one call and leaves
calendar and unit semantics with WeeWX.

Native computed observations project dateTime, temperature and humidity.
The WeeWX bridge also validates usUnits. They evaluate each row's weather
formula and update aggregate states during the same scan, without building
Python records or a vector of intermediate calculated readings. SQL reuses
one computed state for AVG, MIN, MAX, SUM and COUNT of the same expression.
Hourly or calendar series assign calculated values directly to the requested
interval states. Corrections suppress old base rows before formula evaluation.
Computed summaries are query-local; they are not persisted or reused across
queries. Adding a kernel requires a Rust formula and a matching adapter
definition. Existing daily statistics of the input columns cannot answer
nonlinear derived calculations.

The broader analytics path projects each weather kernel's inputs and reduces
them into interval states during one ordered cursor scan. ET uses a second
projected cursor and a sliding window. Monotonic deques retain extrema, while
compensated running sums retain averages; advancing the window does not issue
a SQL query for each output reading. Pressure calculations reuse the provider's
12-hour temperature cache and seek only when it expires. Rain-rate queries
augment the existing LOOP queue once, then use a suffix sum and binary search.
Suffix sums preserve small recent tips after a very large expired tip. Provider
locks cover the detached native request and the state transfer back to Python.

Daily and vector analytics retain their own eligibility, time-weight and NULL
rules. Historical xaggs builds a month/day lookup for the requested output bins
and scans daily summaries once across all years. It preserves the stored LOOP
extrema and timestamps. Exact percentile samples share an operation-wide
allowance instead of allocating a full allowance in every interval.

The solar/lunar implementation is Rust. Numeric reference tables are generated
from the pinned MIT-licensed PyEphem source, with no C astronomy build or runtime
dependency. Event searches normally converge by iterating the target hour angle.
Near polar and grazing horizons they use bracketed sampling and extremum
refinement, which also detects visibility intervals shorter than the sampling
step. The Python almanac adapter caches at most 128 parameterized results;
observer, timestamp, horizon and center/limb choice are part of the key. No
astronomical results are persisted in the database. See the
[astronomy reference](astronomy.md) for supported attributes and source notices.

Tests kill writers during commits and compactions and check that every
acknowledged transaction survives and the file passes `weewx-db check`.
Durability against power failure is SQLite's in WAL mode with
`synchronous=FULL`; it depends on storage that honors `fsync`.
