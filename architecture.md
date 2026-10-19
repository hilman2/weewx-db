# Storage and query execution

For installation and queries, start with the [usage guide](README.md).
This page describes the experimental implementation.

Archive blocks contain at most 4,096 rows, sorted by dateTime. Daily-summary
blocks contain at most 128 rows so changing today's summary rewrites a small
block. Columns hold a NULL bitmap and little-endian 64-bit values. Entirely
NULL columns need no payload. Each nonempty column uses LZ4 when that reduces
its size, and has a CRC over the stored bytes.

The checksummed manifest records table schemas, block time ranges and offsets.
Every column block carries count, compensated sum, min and max. These summaries
answer complete unaffected blocks without decoding them. The time index
selects blocks and binary searches within them. Min/max and NULL counts also
exclude blocks for observation filters; SQL rechecks the remaining rows.
Columns are fetched only when projected. A bounded FIFO cache holds decoded
columns; connections to the same file share it. Schema additions synthesize missing old column blocks from a default.

Changes live in a sorted overlay and a checksummed journal. A transaction
becomes one frame. Readers merge the overlay with base blocks and suppress
replaced or deleted readings before aggregation. The journal is replayed when
opening or refreshing a connection, starting after the frames another
connection of the process has already replayed. A truncated final frame is ignored.
The final frame from a background commit can also be discarded if torn;
corruption in an earlier complete frame remains an error. New journals use
WWXWAL03, whose frame checksum covers the synchronization flag, length,
sequence and payload. Existing WWXWAL02 journals are checkpointed on their
next write. WWXDB001 snapshots remain readable.

Checkpoint appends replacement blocks and a new manifest, synchronizes them,
then publishes a checksummed `.head` pointer by atomic rename and synchronizes
the directory. It resets the journal only after publication. A crash before
publication leaves the previous checkpoint and journal available. A crash
after publication can leave an old journal, whose sequence is recognized.
Vacuum copies live blocks into a synchronized replacement file. Existing
readers keep their old file descriptors until their snapshots finish.

WeeWX opens new connections for every report run. Connections to the same
file in one process therefore share what does not change between runs. A
parsed and validated manifest is kept per file, inode and manifest position.
Decoded column blocks live in one cache per file and inode, because a block
never changes at its offset: checkpoints only append, and vacuum writes a new
file. The replayed journal is shared too. A new connection continues from the
replayed state when the snapshot, journal inode and journal base sequence
match, and replays only the newer frames. Each frame's checksum is therefore
verified once per process; a new process verifies the journal again. Each connection's own uncommitted
changes stay private, because shared rows and schemas are copied on write.

A process-wide change log records which table and time range every published
transaction touched. Frames read during replay and own commits are recorded;
checkpoints and vacuum record no data change. The WeeWX XType adapter keeps
aggregate results per file and database identity, with the time ranges they
read. A later revision reuses a result when the log shows no change in those
ranges since it was computed. A sequence number missing from the log, for
example a frame another process folded into a checkpoint, invalidates the
result. Results are never used inside a transaction, and stateful weather
providers bypass the cache. Spans ending within two days of the newest record
mostly move with the report time; their results stay with one connection and
revision. The other results are kept per database in two generations of
`result_cache` entries.

Files belonging to one database are the main `.wwdb` file and its `.wal`,
`.head` and `.lock` sidecars. Do not copy the main file alone while writes run.
For a portable snapshot, export to a new SQLite file. For a filesystem backup,
stop writers and close connections, then copy the main file and existing
sidecars. The persistent lock file prevents competing writers from locking
different inodes after database removal.

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

The prototype checks corruption and crash boundaries in tests. Those tests
do not establish power-failure behavior for every filesystem or storage device.
