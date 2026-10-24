//! Transactions over immutable, indexed column blocks.
//!
//! A `Store` keeps a snapshot in memory: the block index of the file and the
//! rows changed since the last compaction, which override the blocks.
//! Writes are staged in memory and written to the file in one SQLite
//! transaction at commit. A compaction folds the changed rows into blocks.

use super::*;
use crate::container::Container;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

/// Changed rows of one table that start a compaction before the next write.
const COMPACT_TABLE_ROWS: usize = 1024;
/// Changed rows of all tables that start a compaction.
const COMPACT_ROWS: usize = 16384;
/// Staged bytes after which a transaction is refused. Staged rows are held in
/// memory until commit.
const MAX_TRANSACTION_BYTES: usize = 256 * 1024 * 1024;

/// How a commit reaches the disk. `Always` (SQLite synchronous=FULL) makes
/// every acknowledged transaction survive a power failure. `Background`
/// (synchronous=NORMAL) can lose the transactions since the last WAL
/// checkpoint after a power failure, but never corrupts the file.
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum SyncMode { Always, Background }

impl std::str::FromStr for SyncMode {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        match value { "always"=>Ok(Self::Always),"background"=>Ok(Self::Background),_=>bail!("sync_mode must be always or background") }
    }
}

#[derive(Clone)]
enum Mutation {
    Put { table: String, ordinal: u64, time: i64, row: Option<Vec<Cell>> },
    Create { name: String, sql: String, columns: Vec<Column>, ordinal: u64 },
    Metadata { name: String, sql: String, rows: Vec<Vec<Cell>> },
    Add { table: String, column: Column, sql: String },
    Drop { table: String, ordinal: Option<u64> },
}

impl Mutation {
    fn bytes(&self) -> usize {
        match self {
            Mutation::Put { row, .. } => 32 + row.as_ref().map_or(0, |r| r.len() * 9),
            Mutation::Metadata { rows, .. } => 64 + rows.iter().map(|r| r.len() * 16).sum::<usize>(),
            _ => 1024,
        }
    }
}

/// Changed rows of one table. Cursors share the map; a write copies it only
/// while a cursor still reads the previous version.
pub(crate) type TableChanges = Arc<BTreeMap<i64, Option<Vec<Cell>>>>;
type Changes = BTreeMap<String, TableChanges>;

pub fn sidecar(path: &Path, suffix: &str) -> PathBuf { crate::container::sidecar(path, suffix) }

#[derive(Debug, Serialize)]
pub struct StoreInfo {
    #[serde(flatten)]
    pub snapshot: Info,
    pub format_version: i32,
    pub lsn: u64,
    pub generation: u64,
    pub changed_rows: usize,
    pub indexed_columns: usize,
    pub summarized_blocks: usize,
    pub file_bytes: u64,
    pub free_bytes: u64,
}

pub struct Store {
    pub(crate) snapshot: Database,
    pub(crate) changes: Changes,
    lsn: u64,
    writing: bool,
    pending: Vec<Mutation>,
    pending_bytes: usize,
    timeout: Duration,
    /// PRAGMA data_version when the snapshot was read; another connection's
    /// commit changes it.
    data_version: i64,
    pub workspace_bytes: usize,
    sync_mode: SyncMode,
    /// The next table ordinal, read when a write transaction begins.
    next_table: u64,
    /// Results of `aggregate` for the current snapshot. Reports repeat
    /// queries such as the first and last timestamp many times.
    aggregates: HashMap<(usize, String, i64, i64, bool), Stats>,
    /// Resolved path identifying this database in process-wide state.
    canonical: PathBuf,
}

impl Store {
    pub fn drop_database(path: &Path) -> Result<()> {
        let path = crate::container::resolve(path)?;
        {
            // Refuses a file that is not a database, and waits for a writer.
            let container = Container::open(&path, Duration::from_secs(5), false)?;
            container.begin_write()?;
            container.rollback();
        }
        std::fs::remove_file(&path)?;
        for suffix in ["-wal", "-shm"] {
            match std::fs::remove_file(sidecar(&path, suffix)) {
                Ok(()) => (), Err(e) if e.kind() == std::io::ErrorKind::NotFound => (), Err(e) => return Err(e.into()),
            }
        }
        crate::container::sync_directory(&path)
    }

    /// Create an empty database; tables can then be created transactionally.
    pub fn create(path: &Path, archive: &str) -> Result<()> {
        Container::create(path, archive, |_| Ok(()))
    }

    pub fn open(path: &Path, cache_bytes: usize) -> Result<Self> {
        Self::open_with_timeout(path, cache_bytes, Duration::from_secs(5))
    }

    /// Open, waiting at most `timeout` whenever another writer holds the lock.
    pub fn open_with_timeout(path: &Path, cache_bytes: usize, timeout: Duration) -> Result<Self> {
        Self::open_container(Container::open(path, timeout, false)?, cache_bytes, timeout)
    }

    /// Open without ever writing, also where the directory is read-only.
    pub fn open_read_only(path: &Path, cache_bytes: usize) -> Result<Self> {
        let timeout = Duration::from_secs(5);
        Self::open_container(Container::open(path, timeout, true)?, cache_bytes, timeout)
    }

    fn open_container(container: Container, cache_bytes: usize, timeout: Duration) -> Result<Self> {
        let canonical = container.path.clone();
        let mut store = Self { snapshot: Database::new(container, cache_bytes), changes: BTreeMap::new(), lsn: 0,
            writing: false, pending: vec![], pending_bytes: 0, timeout, data_version: 0, workspace_bytes: 32*1024*1024,
            sync_mode: SyncMode::Always, next_table: 0, aggregates: HashMap::new(), canonical };
        store.reload()?;
        Ok(store)
    }

    pub fn in_transaction(&self) -> bool { self.writing }
    pub fn lsn(&self) -> u64 { self.lsn }
    pub fn set_sync_mode(&mut self, mode: SyncMode) -> Result<()> {
        self.snapshot.container.set_synchronous(mode)?;
        self.sync_mode = mode;
        Ok(())
    }

    /// Make committed transactions durable. Only `Background` defers that,
    /// until a WAL checkpoint.
    pub fn flush(&mut self) -> Result<()> {
        if matches!(self.sync_mode, SyncMode::Background) && !self.writing {
            self.snapshot.container.connection.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()))?;
        }
        Ok(())
    }
    pub fn set_workspace(&mut self, bytes: usize) -> Result<()> {
        ensure!(bytes >= 65536,"workspace must be at least 64 KiB");
        self.workspace_bytes = bytes;
        Ok(())
    }
    pub fn set_timeout(&mut self, seconds: f64) -> Result<()> {
        ensure!(seconds.is_finite() && (0.0..=3600.0).contains(&seconds), "invalid lock timeout");
        self.timeout = Duration::from_secs_f64(seconds);
        self.snapshot.container.set_timeout(self.timeout)
    }

    /// Read what other connections committed since the snapshot was taken.
    pub fn refresh(&mut self) -> Result<()> {
        if self.in_transaction() { return Ok(()); }
        if self.snapshot.container.data_version()? == self.data_version { return Ok(()); }
        self.reload()
    }

    /// Read the snapshot again: the block index (shared by the process per
    /// generation) and the changed rows (continued from what this process
    /// read before, when the generation is the same).
    fn reload(&mut self) -> Result<()> {
        self.aggregates.clear();
        self.pending.clear();
        self.pending_bytes = 0;
        // Inside a write transaction, the reads include its own uncommitted
        // writes, which must not reach process-wide state.
        let committed = !self.writing;
        if committed { self.snapshot.container.begin_read()?; }
        let result = self.read_snapshot(committed);
        if committed { self.snapshot.container.rollback(); }
        result
    }

    fn read_snapshot(&mut self, committed: bool) -> Result<()> {
        let meta = self.snapshot.container.meta()?;
        self.snapshot.load_index(&meta, committed)?;
        let mut base = None;
        if committed && let Ok(deltas) = deltas().lock()
            && let Some(state) = deltas.get(&self.canonical).filter(|s| s.id == meta.uuid && s.generation == meta.generation && s.seq <= meta.seq) {
            base = Some((state.seq, state.changes.clone()));
        }
        let (since, mut changes) = base.unwrap_or_default();
        let rows = self.snapshot.container.load_delta(since)?;
        let tables: HashMap<u64, usize> = self.snapshot.manifest.tables.iter().enumerate().map(|(i, t)| (t.ordinal, i)).collect();
        let mut frames: BTreeMap<u64, Vec<crate::changes::Range>> = BTreeMap::new();
        for (ordinal, time, seq, row) in rows {
            let table = &self.snapshot.manifest.tables[*tables.get(&ordinal).context("changed row of an unknown table")?];
            let row = row.map(|mut row| -> Result<Vec<Cell>> {
                ensure!(row.len() <= table.columns.len(), "changed row of {} has too many cells", table.name);
                // The row was written before a column was added.
                for column in &table.columns[row.len()..] { row.push(column.default.clone().unwrap_or(Cell::Null)); }
                Ok(row)
            }).transpose()?;
            Arc::make_mut(changes.entry(table.name.clone()).or_default()).insert(time, row);
            let ranges = frames.entry(seq).or_default();
            match ranges.iter_mut().find(|(name, ..)| *name == table.name) {
                Some(range) => { range.1 = range.1.min(time); range.2 = range.2.max(time); }
                None => ranges.push((table.name.clone(), time, time)),
            }
        }
        for (seq, ranges) in frames { crate::changes::record(&self.canonical, &meta.uuid, seq, ranges); }
        if committed && let Ok(mut deltas) = deltas().lock() {
            deltas.insert(self.canonical.clone(), DeltaState { id: meta.uuid.clone(), generation: meta.generation, seq: meta.seq, changes: changes.clone() });
        }
        self.changes = changes;
        self.lsn = meta.seq;
        self.data_version = self.snapshot.container.data_version()?;
        Ok(())
    }

    pub fn begin(&mut self) -> Result<()> { self.begin_with(false) }

    /// Begin a transaction without replacing the snapshot. Open read cursors
    /// hold block positions in it, so neither a reload nor the automatic
    /// compaction may run; the compaction waits for a later transaction.
    pub(crate) fn begin_keeping_snapshot(&mut self) -> Result<()> { self.begin_with(true) }

    fn begin_with(&mut self, keep_snapshot: bool) -> Result<()> {
        ensure!(!self.in_transaction(), "transaction already active");
        self.snapshot.container.begin_write()?;
        self.writing = true;
        let result = (|| -> Result<()> {
            let meta = self.snapshot.container.meta()?;
            if (meta.seq, meta.generation, &meta.uuid) != (self.lsn, self.snapshot.manifest.generation, &self.snapshot.manifest.id) {
                ensure!(!keep_snapshot, "another connection changed the database while a read cursor is open; finish reading before writing");
                self.reload()?;
            }
            if !keep_snapshot && self.needs_compaction() { self.compact_locked(false)?; }
            self.next_table = self.snapshot.container.counter("next_table")?;
            Ok(())
        })();
        if let Err(error) = result {
            self.snapshot.container.rollback();
            self.writing = false;
            let _ = self.reload();
            return Err(error);
        }
        Ok(())
    }

    fn needs_compaction(&self) -> bool {
        let rows: usize = self.changes.values().map(|c| c.len()).sum();
        rows >= COMPACT_ROWS || self.changes.values().any(|c| c.len() >= COMPACT_TABLE_ROWS)
    }

    pub fn commit(&mut self) -> Result<()> {
        ensure!(self.in_transaction(), "no active transaction");
        let seq = self.lsn.checked_add(1).context("transaction sequence exhausted")?;
        let written = !self.pending.is_empty();
        if written { self.write_pending(seq)?; }
        self.snapshot.container.commit()?;
        if written {
            crate::changes::record(&self.canonical, &self.snapshot.manifest.id, seq, touched(&self.pending));
            self.lsn = seq;
            Arc::make_mut(&mut self.snapshot.manifest).lsn = seq;
        }
        self.pending.clear(); self.pending_bytes = 0;
        self.writing = false;
        Ok(())
    }

    /// Write the staged mutations as transaction `seq`, in their order.
    fn write_pending(&mut self, seq: u64) -> Result<()> {
        let container = &self.snapshot.container;
        let mut schema = false;
        for operation in &self.pending {
            match operation {
                Mutation::Put { ordinal, time, row, .. } => container.put_delta(*ordinal, *time, seq, row.as_deref())?,
                Mutation::Create { name, .. } | Mutation::Add { table: name, .. } => {
                    schema = true;
                    // The final definition; a later Drop removes it again.
                    if let Some(table) = self.snapshot.manifest.tables.iter().find(|t| t.name == *name) { container.write_table(table)?; }
                }
                Mutation::Metadata { name, .. } => {
                    if let Some(table) = self.snapshot.manifest.metadata.iter().find(|t| t.name == *name) { container.write_metadata(table)?; }
                }
                Mutation::Drop { table, ordinal } => {
                    schema = true;
                    match ordinal {
                        Some(ordinal) => container.drop_table(*ordinal, self.snapshot.manifest.generation + 1)?,
                        None => container.drop_metadata(table)?,
                    }
                }
            }
        }
        if schema {
            let generation = self.snapshot.manifest.generation + 1;
            container.set_counter("generation", generation)?;
            Arc::make_mut(&mut self.snapshot.manifest).generation = generation;
        }
        container.set_counter("next_table", self.next_table)?;
        container.set_counter("seq", seq)?;
        Ok(())
    }

    pub fn rollback(&mut self) -> Result<()> {
        ensure!(self.in_transaction(), "no active transaction");
        self.snapshot.container.rollback();
        self.writing = false;
        self.reload()
    }

    pub(crate) fn savepoint(&self) -> usize { self.pending.len() }

    pub(crate) fn restore_statement(&mut self, point: usize) -> Result<()> {
        let retained = self.pending[..point].to_vec();
        self.reload()?;
        for operation in retained { self.stage(operation)?; }
        Ok(())
    }

    fn stage(&mut self, operation: Mutation) -> Result<()> {
        ensure!(self.in_transaction(), "writes require a transaction");
        let size = operation.bytes();
        ensure!(self.pending_bytes + size <= MAX_TRANSACTION_BYTES, "transaction exceeds 256 MiB; use smaller batches");
        self.apply(&operation)?;
        self.pending_bytes += size;
        self.pending.push(operation);
        Ok(())
    }

    fn apply(&mut self, operation: &Mutation) -> Result<()> {
        self.aggregates.clear();
        match operation {
            Mutation::Put { table, time, row, .. } => {
                self.table_index(table)?;
                Arc::make_mut(self.changes.entry(table.clone()).or_default()).insert(*time, row.clone());
            }
            Mutation::Create { name, sql, columns, ordinal } => {
                ensure!(!self.tables().contains(name), "table already exists: {name}");
                tables_mut(&mut self.snapshot.manifest).push(Table { ordinal: *ordinal, name: name.clone(), sql: sql.clone(), columns: columns.clone(), blocks: vec![], rows: 0 });
            }
            Mutation::Metadata { name, sql, rows } => {
                Arc::make_mut(&mut self.snapshot.manifest).metadata.retain(|t| t.name != *name);
                Arc::make_mut(&mut self.snapshot.manifest).metadata.push(MetadataTable { name: name.clone(), sql: sql.clone(), rows: rows.clone() });
            }
            Mutation::Add { table, column, sql } => {
                let ti = self.table_index(table)?;
                ensure!(!self.snapshot.manifest.tables[ti].columns.iter().any(|c| c.name.eq_ignore_ascii_case(&column.name)), "duplicate column {}", column.name);
                tables_mut(&mut self.snapshot.manifest)[ti].columns.push(column.clone());
                tables_mut(&mut self.snapshot.manifest)[ti].sql = sql.clone();
                if let Some(changes) = self.changes.get_mut(table) {
                    for row in Arc::make_mut(changes).values_mut().flatten() { row.push(column.default.clone().unwrap_or(Cell::Null)); }
                }
            }
            Mutation::Drop { table, .. } => {
                tables_mut(&mut self.snapshot.manifest).retain(|t| t.name != *table);
                Arc::make_mut(&mut self.snapshot.manifest).metadata.retain(|t| t.name != *table);
                self.changes.remove(table);
            }
        }
        Ok(())
    }

    pub fn tables(&self) -> Vec<String> {
        self.snapshot.manifest.tables.iter().map(|t| t.name.clone())
            .chain(self.snapshot.manifest.metadata.iter().map(|t| t.name.clone())).collect()
    }

    pub(crate) fn table_index(&self, name: &str) -> Result<usize> {
        self.snapshot.manifest.tables.iter().position(|t| t.name.eq_ignore_ascii_case(name))
            .with_context(|| format!("no such table: {name}"))
    }

    pub fn columns(&self, table: &str) -> Result<Vec<Column>> {
        Ok(self.column_defs(table)?.to_vec())
    }

    pub(crate) fn column_defs(&self, table: &str) -> Result<&[Column]> {
        Ok(&self.snapshot.manifest.tables[self.table_index(table)?].columns)
    }

    pub fn create_table(&mut self, name: &str, sql: &str, columns: Vec<Column>) -> Result<()> {
        ensure!(columns.iter().any(|c| c.name == "dateTime" && c.kind == Kind::Integer), "numeric tables require an INTEGER dateTime primary key");
        crate::container::validate_definition(sql, name)?;
        let ordinal = self.next_table;
        self.stage(Mutation::Create { name: name.to_owned(), sql: sql.to_owned(), columns, ordinal })?;
        self.next_table += 1;
        Ok(())
    }

    pub fn replace_metadata(&mut self, name: &str, sql: &str, rows: Vec<Vec<Cell>>) -> Result<()> {
        crate::container::validate_definition(sql, name)?;
        self.stage(Mutation::Metadata { name: name.to_owned(), sql: sql.to_owned(), rows })
    }

    pub fn add_column(&mut self, table: &str, column: Column, sql: &str) -> Result<()> {
        ensure!(!column.not_null || column.default.is_some(), "a new NOT NULL column needs a default");
        validate_cell(&column, column.default.as_ref().unwrap_or(&Cell::Null))?;
        self.stage(Mutation::Add { table: table.to_owned(), column, sql: sql.to_owned() })
    }

    pub fn drop_table(&mut self, table: &str) -> Result<()> {
        ensure!(self.tables().contains(&table.to_owned()), "no such table: {table}");
        let ordinal = self.snapshot.manifest.tables.iter().find(|t| t.name == table).map(|t| t.ordinal);
        self.stage(Mutation::Drop { table: table.to_owned(), ordinal })
    }

    pub fn put(&mut self, table: &str, row: Vec<Cell>, replace: bool) -> Result<i64> {
        let columns = self.column_defs(table)?;
        ensure!(row.len() == columns.len(), "wrong column count");
        let mut normalized = Vec::with_capacity(row.len());
        for (column, cell) in columns.iter().zip(row) {
            // A virtual table receives NULL for a column the INSERT omits.
            // A NOT NULL column with a default takes the default, as SQLite
            // does for an omitted column.
            let cell = match (cell, &column.default) {
                (Cell::Null, Some(default)) if column.not_null => default.clone(),
                (cell, _) => numeric_cell(column.kind, cell),
            };
            validate_cell(column, &cell)?;
            normalized.push(cell);
        }
        let index = columns.iter().position(|c| c.name == "dateTime").unwrap();
        let Cell::Integer(time) = normalized[index] else { bail!("dateTime must be an integer") };
        let ti = self.table_index(table)?;
        let (table, ordinal) = (self.snapshot.manifest.tables[ti].name.clone(), self.snapshot.manifest.tables[ti].ordinal);
        let table = table.as_str();
        ensure!(replace || self.get(table, time)?.is_none(), "UNIQUE constraint failed: {table}.dateTime");
        self.stage(Mutation::Put { table: table.to_owned(), ordinal, time, row: Some(normalized) })?;
        Ok(time)
    }

    pub fn put_json(&mut self, table: &str, row: &serde_json::Map<String, serde_json::Value>, replace: bool) -> Result<i64> {
        let columns = self.columns(table)?;
        for name in row.keys() { ensure!(columns.iter().any(|c| c.name == *name), "unknown column {name}; add it explicitly first"); }
        let values = columns.iter().map(|c| match row.get(&c.name) {
            Some(value) => Cell::from_json(value), None => Ok(c.default.clone().unwrap_or(Cell::Null)),
        }).collect::<Result<Vec<_>>>()?;
        self.put(table, values, replace)
    }

    pub fn delete(&mut self, table: &str, time: i64) -> Result<()> {
        let ordinal = self.snapshot.manifest.tables[self.table_index(table)?].ordinal;
        self.stage(Mutation::Put { table: table.to_owned(), ordinal, time, row: None })
    }

    pub fn get(&mut self, table: &str, time: i64) -> Result<Option<Vec<Cell>>> {
        if let Some(row) = self.changes.get(table).and_then(|c| c.get(&time)) { return Ok(row.clone()); }
        let ti = self.table_index(table)?;
        let bi = self.snapshot.manifest.tables[ti].blocks.partition_point(|b| b.last < time);
        if bi == self.snapshot.manifest.tables[ti].blocks.len() { return Ok(None); }
        let rows = self.snapshot.manifest.tables[ti].blocks[bi].rows;
        let ts = self.column_defs(table)?.iter().position(|c| c.name == "dateTime").unwrap();
        let times = self.snapshot.load(ti, bi, ts)?;
        let mut low = 0; let mut high = rows;
        while low < high {
            let mid = low+(high-low)/2;
            if timestamp(&times,rows,mid) < time { low = mid+1; } else { high = mid; }
        }
        if low < rows && timestamp(&times,rows,low) == time {
            let kinds = self.column_defs(table)?.iter().map(|c|c.kind).collect::<Vec<_>>();
            let row = kinds.into_iter().enumerate().map(|(ci,kind)| {
                Ok(decode(&self.snapshot.load(ti,bi,ci)?,rows,low,kind))
            }).collect::<Result<_>>()?;
            return Ok(Some(row));
        }
        Ok(None)
    }

    fn read_block(&mut self, ti: usize, bi: usize) -> Result<Vec<Vec<Cell>>> {
        let rows = self.snapshot.manifest.tables[ti].blocks[bi].rows;
        let kinds = self.snapshot.manifest.tables[ti].columns.iter().map(|c| c.kind).collect::<Vec<_>>();
        let data = (0..kinds.len()).map(|ci| self.snapshot.load(ti, bi, ci)).collect::<Result<Vec<_>>>()?;
        Ok((0..rows).map(|ri| data.iter().zip(&kinds).map(|(bytes, kind)| decode(bytes, rows, ri, *kind)).collect()).collect())
    }

    pub fn aggregate(&mut self, table: &str, column: &str, span: Span) -> Result<Stats> {
        let key = (self.table_index(table)?, column.to_owned(), span.start, span.stop, span.daily);
        if let Some(stats) = self.aggregates.get(&key) { return Ok(stats.clone()); }
        let stats = self.aggregate_uncached(table, column, span)?;
        if self.aggregates.len() >= 4096 { self.aggregates.clear(); }
        self.aggregates.insert(key, stats.clone());
        Ok(stats)
    }

    fn aggregate_uncached(&mut self, table: &str, column: &str, span: Span) -> Result<Stats> {
        let ci = self.column_defs(table)?.iter().position(|c| c.name == column).context("no such column")?;
        let changes = self.changes.get(table).cloned();
        let mut answer = Accumulator::default();
        answer.merge(&self.snapshot.aggregate_excluding(table, column, span, changes.as_deref())?);
        if let Some(changes) = self.changes.get(table) {
            for (&time, row) in changes.iter() {
                if !span.before(time) && !span.after(time) && let Some(row) = row { answer.add(row[ci].numeric()); }
            }
        }
        Ok(answer.finish())
    }

    pub fn buckets(&mut self, table: &str, column: &str, span: Span, seconds: i64) -> Result<Vec<Stats>> {
        ensure!(!span.daily && seconds > 0 && span.start <= span.stop, "invalid bucket span");
        let length = span.stop as i128 - span.start as i128;
        let count = (length + seconds as i128 - 1) / seconds as i128;
        ensure!(count <= (self.workspace_bytes / (std::mem::size_of::<Accumulator>()+std::mem::size_of::<Stats>())) as i128, "bucket workspace exceeded");
        let mut result = vec![Accumulator::default(); count as usize];
        self.scan_values(table,column,span,|time,value| {
            let index = ((time as i128 - span.start as i128 - 1) / seconds as i128) as usize;
            result[index].add(value);
        })?;
        Ok(result.into_iter().map(Accumulator::finish).collect())
    }

    pub fn daily_mean(&mut self, table: &str, start: i64, stop: i64) -> Result<Option<f64>> {
        let span = Span { start, stop, daily: true };
        let sum = self.aggregate(table, "wsum", span)?.sum;
        let weight = self.aggregate(table, "sumtime", span)?.sum;
        Ok(sum.zip(weight).and_then(|(a,b)| (b != 0.0).then_some(a/b)))
    }

    fn scan_values(&mut self, table: &str, column: &str, span: Span, mut visit: impl FnMut(i64,Option<f64>)) -> Result<()> {
        let ci = self.column_defs(table)?.iter().position(|c|c.name == column).context("unknown column")?;
        let changes = self.changes.get(table);
        self.snapshot.scan(table,column,span,|time,value| {
            if changes.is_none_or(|c|!c.contains_key(&time)) { visit(time,value); }
        })?;
        if let Some(changes) = changes {
            for (&time,row) in changes.iter() {
                if !span.before(time) && !span.after(time) && let Some(row) = row { visit(time,row[ci].numeric()); }
            }
        }
        Ok(())
    }

    pub fn stats_ranges(&mut self, table: &str, column: &str, ranges: &[(i64,i64)]) -> Result<Vec<Stats>> {
        ensure!(ranges.len() <= self.workspace_bytes / (std::mem::size_of::<Accumulator>()+std::mem::size_of::<Stats>()+32),"interval workspace exceeded");
        if ranges.is_empty() { return Ok(vec![]); }
        for (i,(start,stop)) in ranges.iter().enumerate() {
            ensure!(start <= stop && (i == 0 || ranges[i-1].1 <= *start),"intervals must be ordered and disjoint");
        }
        let span = Span {start:ranges[0].0,stop:ranges.last().unwrap().1,daily:false};
        let mut bins = vec![Accumulator::default();ranges.len()];
        let mut index = 0;
        self.scan_values(table,column,span,|time,value| {
            // Journal corrections follow the base scan and can precede its last
            // timestamp. Restart the interval search at that merge boundary.
            if index == ranges.len() || time <= ranges[index].0 { index = ranges.partition_point(|r|r.1 < time); }
            while index < ranges.len() && time > ranges[index].1 { index += 1; }
            if index < ranges.len() && time > ranges[index].0 { bins[index].add(value); }
        })?;
        Ok(bins.into_iter().map(Accumulator::finish).collect())
    }

    pub fn series_ranges(&mut self, table: &str, column: &str, ranges: &[(i64,i64)], aggregate: &str) -> Result<Vec<Cell>> {
        ensure!(["sum","avg","min","max","count","cumulative"].contains(&aggregate),"unsupported native series aggregate");
        let bins = self.stats_ranges(table,column,ranges)?;
        let mut total = 0.0;
        Ok(bins.into_iter().map(|stats| {
            if aggregate == "count" { return Cell::Integer(stats.count as i64); }
            let value = match aggregate {
                "sum" => stats.sum, "avg" => stats.sum.map(|s|s/stats.count as f64),
                "min" => stats.min, "max" => stats.max,
                _ => { total += stats.sum.unwrap_or(0.0); Some(total) },
            };
            value.map(|v|Cell::Real(v.to_bits())).unwrap_or(Cell::Null)
        }).collect())
    }

    pub fn raw_series(&mut self, table: &str, column: &str, start: i64, stop: i64) -> Result<(Vec<i64>,Vec<i64>,Vec<Cell>)> {
        let columns = self.columns(table)?;
        let ci = columns.iter().position(|c|c.name == column).context("unknown column")?;
        let interval = columns.iter().position(|c|c.name == "interval").context("interval column missing")?;
        let units = columns.iter().position(|c|c.name == "usUnits");
        let mut cursor = RowCursor::new(self,table,Span {start,stop,daily:false})?;
        let mut starts = Vec::new(); let mut stops = Vec::new(); let mut values = Vec::new();
        let mut unit_system = None;
        while cursor.advance(self)? {
            ensure!(values.len() < self.workspace_bytes / (std::mem::size_of::<Cell>()+16),"raw series workspace exceeded; increase workspace_mib or aggregate the time span");
            if let Some(units) = units {
                let unit = cursor.cell(self,units)?;
                ensure!(unit_system.as_ref().is_none_or(|u|*u == unit),"unit system changes within series");
                unit_system = Some(unit);
            }
            let Cell::Integer(minutes) = cursor.cell(self,interval)? else { bail!("invalid archive interval") };
            let seconds = minutes.checked_mul(60).context("archive interval overflow")?;
            starts.push(cursor.time.checked_sub(seconds).context("archive timestamp overflow")?);
            stops.push(cursor.time);
            values.push(cursor.cell(self,ci)?);
        }
        Ok((starts,stops,values))
    }

    pub fn info(&mut self) -> Result<StoreInfo> {
        let mut info = self.snapshot.info();
        if let Some(changes) = self.changes.get(&info.archive).cloned() {
            let ti = self.table_index(&info.archive)?;
            let ts = self.column_defs(&info.archive)?.iter().position(|c|c.name == "dateTime").unwrap();
            for (&time,row) in changes.iter() {
                let blocks = &self.snapshot.manifest.tables[ti].blocks;
                let bi = blocks.partition_point(|b|b.last < time);
                let exists = if bi < blocks.len() && time >= blocks[bi].first {
                    let rows = blocks[bi].rows;
                    let times = self.snapshot.load(ti,bi,ts)?;
                    (0..rows).any(|ri|timestamp(&times,rows,ri) == time)
                } else { false };
                if !exists && row.is_some() { info.rows += 1; }
                if exists && row.is_none() { info.rows -= 1; }
            }
            for reverse in [false,true] {
                let mut edge = None;
                let count = self.snapshot.manifest.tables[ti].blocks.len();
                for n in 0..count {
                    let bi = if reverse { count-1-n } else { n };
                    let rows = self.snapshot.manifest.tables[ti].blocks[bi].rows;
                    let times = self.snapshot.load(ti,bi,ts)?;
                    for n in 0..rows {
                        let ri = if reverse { rows-1-n } else { n };
                        let time = timestamp(&times,rows,ri);
                        if changes.get(&time).is_none_or(|r|r.is_some()) { edge = Some(time); break; }
                    }
                    if edge.is_some() { break; }
                }
                let journal = if reverse { changes.iter().rev().find(|(_,r)|r.is_some()) } else { changes.iter().find(|(_,r)|r.is_some()) };
                if let Some((&time,_)) = journal { edge = Some(edge.map_or(time,|t|if reverse { t.max(time) } else { t.min(time) })); }
                if reverse { info.last = edge; } else { info.first = edge; }
            }
        }
        let pragma = |name: &str| -> Result<u64> {
            Ok(self.snapshot.container.connection.query_row(&format!("PRAGMA {name}"), [], |r| r.get::<_, i64>(0))? as u64)
        };
        let page = pragma("page_size")?;
        Ok(StoreInfo { snapshot: info, format_version: crate::container::FORMAT_VERSION, lsn: self.lsn,
            generation: self.snapshot.manifest.generation,
            changed_rows: self.changes.values().map(|c| c.len()).sum(),
            indexed_columns: self.snapshot.manifest.tables.iter().map(|t| t.columns.len()).sum(),
            summarized_blocks: self.snapshot.manifest.tables.iter().flat_map(|t| &t.blocks)
                .flat_map(|b| &b.columns).filter(|c| c.stats.is_some()).count(),
            file_bytes: pragma("page_count")? * page, free_bytes: pragma("freelist_count")? * page })
    }

    /// Fold all changed rows into blocks now. `rebuild_stats` also recomputes
    /// the summaries of unchanged blocks.
    pub fn checkpoint(&mut self, rebuild_stats: bool) -> Result<()> {
        ensure!(!self.in_transaction(), "checkpoint cannot run inside a transaction");
        self.begin()?;
        if let Err(error) = self.compact_locked(rebuild_stats) {
            let _ = self.rollback();
            return Err(error);
        }
        self.commit()
    }

    /// Fold the changed rows into blocks inside the current write
    /// transaction. Only blocks with changes, and the last block of a table
    /// when rows follow it, are rewritten; new rows fill the last block up to
    /// its capacity before a new block starts. Replaced chunks become garbage
    /// of the new generation and are deleted one compaction later.
    fn compact_locked(&mut self, rebuild_stats: bool) -> Result<()> {
        let generation = self.snapshot.manifest.generation + 1;
        let seq = self.lsn.checked_add(1).context("transaction sequence exhausted")?;
        let mut next_chunk = self.snapshot.container.counter("next_chunk")?;
        let mut manifest = (*self.snapshot.manifest).clone();
        let mut tables = (*manifest.tables).clone();
        let mut garbage = Vec::new();
        for (ti, table) in tables.iter_mut().enumerate() {
            let changes = self.changes.get(&table.name).cloned().unwrap_or_default();
            if changes.is_empty() && !rebuild_stats { continue; }
            let limit = rows_per_block(&table.name);
            let ts = table.columns.iter().position(|c| c.name == "dateTime").unwrap();
            let count = table.blocks.len();
            let mut blocks = Vec::with_capacity(count + 1);
            let mut lower = std::ops::Bound::Unbounded;
            for bi in 0..count {
                let block = table.blocks[bi].clone();
                // Rows after the last block go into it while it has room.
                let absorbs_tail = bi + 1 == count && block.rows < limit;
                let upper = if absorbs_tail { std::ops::Bound::Unbounded } else { std::ops::Bound::Included(block.last) };
                let changed = changes.range((lower, upper)).collect::<Vec<_>>();
                lower = match changes.keys().next_back() {
                    Some(&newest) if absorbs_tail => std::ops::Bound::Excluded(newest.max(block.last)),
                    _ => std::ops::Bound::Excluded(block.last),
                };
                if changed.is_empty() {
                    let mut block = block;
                    if rebuild_stats {
                        for (ci, column) in block.columns.iter_mut().enumerate() {
                            let bytes = self.snapshot.load(ti, bi, ci)?;
                            let mut stats = Accumulator::default();
                            for ri in 0..block.rows { stats.add(number(&bytes, block.rows, ri, table.columns[ci].kind)); }
                            column.stats = Some(stats.finish());
                        }
                    }
                    blocks.push(block);
                    continue;
                }
                let mut rows = BTreeMap::new();
                for row in self.read_block(ti, bi)? {
                    let Cell::Integer(time) = row[ts] else { bail!("invalid timestamp") };
                    rows.insert(time, row);
                }
                for (&time, row) in changed { match row { Some(row) => { rows.insert(time, row.clone()); }, None => { rows.remove(&time); } } }
                garbage.extend(block.columns.iter().filter(|c| c.chunk > 0).map(|c| c.chunk));
                let rows = rows.into_values().collect::<Vec<_>>();
                for batch in rows.chunks(limit) { write_block(&self.snapshot.container, &mut next_chunk, &table.columns, batch, &mut blocks)?; }
            }
            let tail = changes.range((lower, std::ops::Bound::Unbounded)).filter_map(|(_, row)| row.clone()).collect::<Vec<_>>();
            for batch in tail.chunks(limit) { write_block(&self.snapshot.container, &mut next_chunk, &table.columns, batch, &mut blocks)?; }
            // Summaries rebuilt in place keep their chunks, so every row is written.
            let old = if rebuild_stats { &[][..] } else { &table.blocks[..] };
            self.snapshot.container.write_blocks(table.ordinal, old, &blocks)?;
            table.rows = blocks.iter().map(|b| b.rows as u64).sum();
            table.blocks = blocks;
        }
        let container = &self.snapshot.container;
        container.discard(&garbage, generation)?;
        container.purge(generation - 1)?;
        container.clear_delta()?;
        container.set_counter("next_chunk", next_chunk)?;
        container.set_counter("generation", generation)?;
        container.set_counter("seq", seq)?;
        manifest.tables = Arc::new(tables);
        manifest.generation = generation;
        manifest.lsn = seq;
        self.snapshot.manifest = Arc::new(manifest);
        self.changes.clear();
        self.aggregates.clear();
        self.lsn = seq;
        // A compaction republishes the same rows under a new sequence number.
        crate::changes::record(&self.canonical, &self.snapshot.manifest.id, seq, vec![]);
        Ok(())
    }

    /// Check the file and every block: SQLite's structure, each chunk's
    /// checksum and length, timestamps that are present, ordered and inside
    /// their block's bounds, and the stored summaries.
    pub fn verify(&mut self) -> Result<()> {
        let problems: Vec<String> = self.snapshot.container.connection.prepare("PRAGMA integrity_check")?
            .query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<_>>()?;
        ensure!(problems == ["ok"], "file structure damaged: {}", problems.join("; "));
        for ti in 0..self.snapshot.manifest.tables.len() {
            let table = self.snapshot.manifest.tables[ti].clone();
            let ts = table.columns.iter().position(|c| c.name == "dateTime").unwrap();
            for (bi, block) in table.blocks.iter().enumerate() {
                let times = self.snapshot.load(ti, bi, ts)?;
                let mut previous = None;
                for ri in 0..block.rows {
                    let time = bits(&times, block.rows, ri).map(|t| t as i64)
                        .with_context(|| format!("{}: NULL dateTime in block {}", table.name, block.first))?;
                    ensure!(previous.is_none_or(|p| time > p) && (block.first..=block.last).contains(&time),
                        "{}: dateTime {time} out of order in block {}", table.name, block.first);
                    previous = Some(time);
                }
                for ci in 0..table.columns.len() {
                    let bytes = self.snapshot.load(ti, bi, ci)?;
                    let Some(stored) = block.columns.get(ci).and_then(|c| c.stats.clone()) else { continue };
                    let mut stats = Accumulator::default();
                    for ri in 0..block.rows { stats.add(number(&bytes, block.rows, ri, table.columns[ci].kind)); }
                    let stats = stats.finish();
                    ensure!(stats.count == stored.count && stats.min.map(f64::to_bits) == stored.min.map(f64::to_bits)
                        && stats.max.map(f64::to_bits) == stored.max.map(f64::to_bits),
                        "{}.{}: summary of block {} does not match its values", table.name, table.columns[ci].name, block.first);
                }
            }
        }
        Ok(())
    }

    /// Compact, delete all garbage and let SQLite rebuild the file without
    /// free pages. Readers in other processes that still hold an older index
    /// fail their current query and succeed on the next.
    pub fn vacuum(&mut self) -> Result<()> {
        ensure!(!self.in_transaction(), "vacuum cannot run inside a transaction");
        self.begin()?;
        let result = self.compact_locked(false).and_then(|_| self.snapshot.container.purge(i64::MAX as u64));
        if let Err(error) = result { let _ = self.rollback(); return Err(error); }
        self.commit()?;
        self.snapshot.container.connection.execute_batch("VACUUM")?;
        self.reload()
    }

    /// Write a consistent copy of the committed database to `destination`,
    /// which must not exist. Works while other connections write.
    pub fn backup(&mut self, destination: &Path) -> Result<()> {
        ensure!(!self.in_transaction(), "backup cannot run inside a transaction");
        let destination = crate::container::resolve(destination)?;
        ensure!(!destination.exists(), "destination must not already exist: {}", destination.display());
        let temporary = sidecar(&destination, &format!(".tmp-{}", new_id()?));
        let result = (|| -> Result<()> {
            self.snapshot.container.connection.execute("VACUUM INTO ?", [temporary.to_string_lossy()])?;
            File::open(&temporary)?.sync_all()?;
            std::fs::hard_link(&temporary, &destination).with_context(|| format!("destination must not already exist: {}", destination.display()))?;
            std::fs::remove_file(&temporary)?;
            crate::container::sync_directory(&destination)
        })();
        if result.is_err() { let _ = std::fs::remove_file(&temporary); }
        result
    }

    pub fn export_sqlite(&mut self, destination: &Path) -> Result<()> {
        let output = tempfile::NamedTempFile::new_in(destination.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")))?;
        {
            let mut connection = Connection::open(output.path())?;
            let transaction = connection.transaction()?;
            let manifest = self.snapshot.manifest.clone();
            for table in manifest.tables.iter() {
                transaction.execute_batch(&table.sql)?;
                let mut insert = transaction.prepare(&format!("INSERT INTO {} VALUES ({})",quote(&table.name),vec!["?";table.columns.len()].join(",")))?;
                let mut cursor = RowCursor::new_bounds(self,&table.name,None,None)?;
                while cursor.advance(self)? {
                    let values = (0..table.columns.len()).map(|ci|cursor.cell(self,ci).map(|v|v.value())).collect::<Result<Vec<_>>>()?;
                    insert.execute(params_from_iter(values))?;
                }
            }
            for table in &manifest.metadata {
                transaction.execute_batch(&table.sql)?;
                for row in &table.rows {
                    transaction.execute(&format!("INSERT INTO {} VALUES ({})",quote(&table.name),vec!["?";row.len()].join(",")),params_from_iter(row.iter().map(Cell::value)))?;
                }
            }
            transaction.commit()?;
        }
        crate::publish_file(output, destination)
    }
}

/// Changed rows a process has read for one database, shared by its new
/// connections. They continue from here instead of reading every changed
/// row again, while the generation is the same.
#[derive(Clone)]
struct DeltaState {
    id: String,
    generation: u64,
    seq: u64,
    changes: Changes,
}

fn deltas() -> &'static std::sync::Mutex<HashMap<PathBuf, DeltaState>> {
    static DELTAS: std::sync::OnceLock<std::sync::Mutex<HashMap<PathBuf, DeltaState>>> = std::sync::OnceLock::new();
    DELTAS.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// The tables and time ranges a transaction's mutations change.
fn touched(operations: &[Mutation]) -> Vec<crate::changes::Range> {
    let mut ranges: Vec<crate::changes::Range> = Vec::new();
    for operation in operations {
        let (table, first, last) = match operation {
            Mutation::Put { table, time, .. } => (table, *time, *time),
            Mutation::Create { name, .. } | Mutation::Metadata { name, .. } => (name, i64::MIN, i64::MAX),
            Mutation::Add { table, .. } | Mutation::Drop { table, .. } => (table, i64::MIN, i64::MAX),
        };
        match ranges.iter_mut().find(|(name, ..)| name == table) {
            Some(range) => { range.1 = range.1.min(first); range.2 = range.2.max(last); }
            None => ranges.push((table.clone(), first, last)),
        }
    }
    ranges
}

impl Store {
    /// Whether the transactions after `since` left `table` within `lo..=hi`
    /// unchanged. False inside a transaction or when this process cannot tell.
    pub fn unchanged_since(&self, since: u64, table: &str, lo: i64, hi: i64) -> bool {
        !self.in_transaction() && crate::changes::unchanged(&self.canonical, &self.snapshot.manifest.id, since, self.lsn, table, lo, hi)
    }
}

/// Accept what a numeric SQLite column would store as a number: INTEGER
/// columns also hold REAL values, and REAL values may be infinite. NaN
/// arrives as NULL, as SQLite binds it.
pub(crate) fn validate_cell(column: &Column, value: &Cell) -> Result<()> {
    ensure!(!column.not_null || *value != Cell::Null, "NOT NULL constraint failed: {}", column.name);
    ensure!(!matches!(value, Cell::Text(_) | Cell::Blob(_)), "datatype mismatch: {} holds numbers", column.name);
    ensure!(column.name != "dateTime" || matches!(value, Cell::Integer(_)), "datatype mismatch: dateTime must be an integer");
    Ok(())
}

fn decode(bytes: &[u8], rows: usize, ri: usize, kind: Kind) -> Cell { cell_at(bytes, rows, ri, kind) }

fn tables_mut(manifest: &mut Arc<Manifest>) -> &mut Vec<Table> { Arc::make_mut(&mut Arc::make_mut(manifest).tables) }

/// Encode `rows` as one block, store its chunks and append it to `blocks`.
pub(crate) fn write_block(container: &Container, next_chunk: &mut u64, columns: &[Column], rows: &[Vec<Cell>], blocks: &mut Vec<Block>) -> Result<()> {
    let ts = columns.iter().position(|c| c.name == "dateTime").context("dateTime missing")?;
    let times = rows.iter().map(|r| match r[ts] { Cell::Integer(n) => Ok(n), _ => bail!("invalid timestamp") }).collect::<Result<Vec<_>>>()?;
    let mut block = Block { rows: rows.len(), first: times[0], last: *times.last().unwrap(), columns: Vec::with_capacity(columns.len()) };
    for (ci, column) in columns.iter().enumerate() {
        let cells: Vec<&Cell> = rows.iter().map(|row| &row[ci]).collect();
        let mut stats = Accumulator::default();
        for cell in &cells { stats.add(cell.numeric()); }
        let (bytes, compressed) = stored_column(encode_cells(column.kind, &cells)?);
        let chunk = if bytes.is_empty() { 0 } else {
            let id = *next_chunk;
            *next_chunk += 1;
            container.write_chunk(id, &bytes)?;
            id
        };
        block.columns.push(ColumnBlock { chunk, length: bytes.len(), checksum: crc32fast::hash(&bytes), stats: Some(stats.finish()), compressed });
    }
    blocks.push(block);
    Ok(())
}

/// Delivery order of a cursor. `Times` delivers a precomputed list, used for
/// ORDER BY on a measurement column.
enum Order { Forward, Reverse { entered: bool }, Times(Vec<i64>, usize) }

/// A cursor keeps at most one block's projected columns and the small journal
/// overlay. It never materializes the historical archive to execute SQL,
/// except for the bounded time list of an ORDER BY ... LIMIT.
pub(crate) struct RowCursor {
    pub time: i64,
    ti: usize,
    ts: usize,
    lower: Option<(i64, bool)>,
    upper: Option<(i64, bool)>,
    /// Forward: next block. Reverse: blocks before this index remain.
    bi: usize,
    /// Forward: next row. Reverse: rows before this index remain.
    ri: usize,
    loaded_bi: usize,
    columns: Vec<Option<Arc<[u8]>>>,
    overlay: Option<TableChanges>,
    /// Journal rows up to this time, in delivery order, are done.
    overlay_done: Option<i64>,
    current: Option<(usize, usize)>,
    overlay_row: Option<i64>,
    order: Order,
    /// Column kinds and the row count of the current block, so that loaded
    /// columns can be decoded without the store.
    kinds: Vec<Kind>,
    rows: usize,
    pub pruning: Vec<(usize, String, Option<f64>)>,
}

impl RowCursor {
    pub fn new(store: &Store, table: &str, span: Span) -> Result<Self> {
        Self::new_bounds(store, table, Some((span.start, span.daily)), Some((span.stop, !span.daily)))
    }

    pub fn new_bounds(store: &Store, table: &str, lower: Option<(i64,bool)>, upper: Option<(i64,bool)>) -> Result<Self> {
        let ti = store.table_index(table)?;
        let ts = store.snapshot.manifest.tables[ti].columns.iter().position(|c| c.name == "dateTime").unwrap();
        let bi = store.snapshot.manifest.tables[ti].blocks.partition_point(|b| before(lower, b.last));
        let name = &store.snapshot.manifest.tables[ti].name;
        let overlay = store.changes.get(name).filter(|c| !c.is_empty()).cloned();
        let kinds: Vec<Kind> = store.snapshot.manifest.tables[ti].columns.iter().map(|c| c.kind).collect();
        Ok(Self { time: 0, ti, ts, lower, upper, bi, ri: 0, loaded_bi: usize::MAX, columns: vec![None; kinds.len()],
            overlay, overlay_done: None, current: None, overlay_row: None, order: Order::Forward, kinds, rows: 0, pruning: vec![] })
    }

    /// Deliver rows by descending dateTime.
    pub fn new_reverse(store: &Store, table: &str, lower: Option<(i64,bool)>, upper: Option<(i64,bool)>) -> Result<Self> {
        let mut cursor = Self::new_bounds(store, table, lower, upper)?;
        cursor.bi = store.snapshot.manifest.tables[cursor.ti].blocks.partition_point(|b| !after(upper, b.first));
        cursor.order = Order::Reverse { entered: false };
        Ok(cursor)
    }

    /// Deliver exactly these rows in this order. Every time must exist.
    pub fn new_times(store: &Store, table: &str, times: Vec<i64>) -> Result<Self> {
        let mut cursor = Self::new_bounds(store, table, None, None)?;
        cursor.order = Order::Times(times, 0);
        Ok(cursor)
    }

    fn pruned(&self, store: &Store, bi: usize) -> bool {
        self.pruning.iter().any(|(ci,op,value)| {
            let Some(stats) = store.snapshot.block_summary(self.ti,bi,*ci) else { return false };
            let rows = store.snapshot.manifest.tables[self.ti].blocks[bi].rows as u64;
            match (op.as_str(), value) {
                ("null",_) => stats.count == rows, ("notnull",_) => stats.count == 0,
                (_,_) if stats.count == 0 => true,
                ("eq",Some(v)) => stats.min.is_some_and(|min| min > *v) || stats.max.is_some_and(|max| max < *v),
                ("gt",Some(v)) => stats.max.is_some_and(|max| max <= *v),
                ("ge",Some(v)) => stats.max.is_some_and(|max| max < *v),
                ("lt",Some(v)) => stats.min.is_some_and(|min| min >= *v),
                ("le",Some(v)) => stats.min.is_some_and(|min| min > *v), _ => false,
            }
        })
    }

    fn enter(&mut self, store: &mut Store, bi: usize) -> Result<Arc<[u8]>> {
        if self.loaded_bi != bi {
            self.columns.iter_mut().for_each(|c| *c = None); self.loaded_bi = bi;
            self.rows = store.snapshot.manifest.tables[self.ti].blocks[bi].rows;
        }
        if self.columns[self.ts].is_none() {
            self.columns[self.ts] = Some(store.snapshot.load(self.ti, bi, self.ts)?);
        }
        Ok(self.columns[self.ts].clone().unwrap())
    }

    fn base_next(&mut self, store: &mut Store) -> Result<Option<(i64, usize, usize)>> {
        while self.bi < store.snapshot.manifest.tables[self.ti].blocks.len() {
            if self.loaded_bi != self.bi && self.pruned(store, self.bi) {
                store.snapshot.metrics.skipped_blocks += 1;
                self.bi += 1; self.ri = 0; continue;
            }
            let block = &store.snapshot.manifest.tables[self.ti].blocks[self.bi];
            if after(self.upper, block.first) { return Ok(None); }
            let n = block.rows;
            let entering = self.loaded_bi != self.bi;
            let times = self.enter(store, self.bi)?;
            if entering {
                let mut first = 0; let mut stop = n;
                while first < stop {
                    let middle = first + (stop-first)/2;
                    if before(self.lower,timestamp(&times,n,middle)) { first = middle+1; } else { stop = middle; }
                }
                self.ri = first;
            }
            while self.ri < n {
                let time = timestamp(&times, n, self.ri);
                if after(self.upper, time) { return Ok(None); }
                if !before(self.lower, time) && !self.overlay.as_ref().is_some_and(|o| o.contains_key(&time)) { return Ok(Some((time, self.bi, self.ri))); }
                self.ri += 1;
            }
            self.bi += 1; self.ri = 0;
        }
        Ok(None)
    }

    fn base_previous(&mut self, store: &mut Store) -> Result<Option<(i64, usize, usize)>> {
        while self.bi > 0 {
            let bi = self.bi - 1;
            let entered = matches!(self.order, Order::Reverse { entered: true });
            if !entered && self.pruned(store, bi) {
                store.snapshot.metrics.skipped_blocks += 1;
                self.bi -= 1; continue;
            }
            let block = &store.snapshot.manifest.tables[self.ti].blocks[bi];
            if before(self.lower, block.last) { return Ok(None); }
            let n = block.rows;
            let times = self.enter(store, bi)?;
            if !entered {
                let mut first = 0; let mut stop = n;
                while first < stop {
                    let middle = first + (stop-first)/2;
                    if !after(self.upper,timestamp(&times,n,middle)) { first = middle+1; } else { stop = middle; }
                }
                self.ri = first;
                self.order = Order::Reverse { entered: true };
            }
            while self.ri > 0 {
                let time = timestamp(&times, n, self.ri - 1);
                if before(self.lower, time) { return Ok(None); }
                if !self.overlay.as_ref().is_some_and(|o| o.contains_key(&time)) { return Ok(Some((time, bi, self.ri - 1))); }
                self.ri -= 1;
            }
            self.bi -= 1;
            self.order = Order::Reverse { entered: false };
        }
        Ok(None)
    }

    fn deliver_base(&mut self, time: i64, bi: usize, ri: usize) {
        // The block was entered while finding this row, so its columns and
        // row count are current.
        debug_assert_eq!(self.loaded_bi, bi);
        self.time = time; self.current = Some((bi, ri)); self.overlay_row = None;
    }

    /// The current cell if it can be read without the store: a journal row or
    /// an already loaded column.
    pub fn loaded_cell(&self, ci: usize) -> Option<Cell> {
        if let Some(time) = self.overlay_row {
            return self.overlay.as_ref()?.get(&time)?.as_ref().map(|row| row[ci].clone());
        }
        let (_, ri) = self.current?;
        Some(decode(self.columns.get(ci)?.as_ref()?, self.rows, ri, self.kinds[ci]))
    }

    fn deliver_overlay(&mut self, time: i64) {
        self.time = time; self.overlay_row = Some(time); self.current = None;
    }

    pub fn advance(&mut self, store: &mut Store) -> Result<bool> {
        use std::ops::Bound::{Excluded, Included, Unbounded};
        match self.order {
            Order::Times(..) => self.advance_times(store),
            Order::Reverse { .. } => {
                let base = self.base_previous(store)?;
                if let Some(overlay) = &self.overlay {
                    let end = match self.overlay_done {
                        Some(done) => Excluded(done),
                        None => match self.upper { Some((t, true)) => Included(t), Some((t, false)) => Excluded(t), None => Unbounded },
                    };
                    let mut found = None;
                    for (&time, row) in overlay.range((Unbounded, end)).rev() {
                        if before(self.lower, time) || base.is_some_and(|(t,_,_)| time <= t) { break; }
                        self.overlay_done = Some(time);
                        if row.is_some() { found = Some(time); break; }
                    }
                    if let Some(time) = found { self.deliver_overlay(time); return Ok(true); }
                }
                if let Some((time, bi, ri)) = base {
                    self.deliver_base(time, bi, ri);
                    self.ri = ri;
                    return Ok(true);
                }
                Ok(false)
            }
            Order::Forward => {
                let base = self.base_next(store)?;
                if let Some(overlay) = &self.overlay {
                    let start = match self.overlay_done {
                        Some(done) => Excluded(done),
                        None => match self.lower { Some((t, true)) => Included(t), Some((t, false)) => Excluded(t), None => Unbounded },
                    };
                    let mut found = None;
                    for (&time, row) in overlay.range((start, Unbounded)) {
                        if after(self.upper, time) || base.is_some_and(|(t,_,_)| time >= t) { break; }
                        self.overlay_done = Some(time);
                        if row.is_some() { found = Some(time); break; }
                    }
                    if let Some(time) = found { self.deliver_overlay(time); return Ok(true); }
                }
                if let Some((time, bi, ri)) = base {
                    self.deliver_base(time, bi, ri);
                    self.ri += 1;
                    return Ok(true);
                }
                Ok(false)
            }
        }
    }

    fn advance_times(&mut self, store: &mut Store) -> Result<bool> {
        let Order::Times(times, index) = &mut self.order else { unreachable!() };
        let Some(&time) = times.get(*index) else { return Ok(false) };
        *index += 1;
        if self.overlay.as_ref().is_some_and(|o| o.contains_key(&time)) {
            self.deliver_overlay(time);
            return Ok(true);
        }
        let bi = store.snapshot.manifest.tables[self.ti].blocks.partition_point(|b| b.last < time);
        ensure!(bi < store.snapshot.manifest.tables[self.ti].blocks.len(), "ordered row missing");
        let n = store.snapshot.manifest.tables[self.ti].blocks[bi].rows;
        let times = self.enter(store, bi)?;
        let (mut first, mut stop) = (0, n);
        while first < stop {
            let middle = first + (stop-first)/2;
            if timestamp(&times,n,middle) < time { first = middle+1; } else { stop = middle; }
        }
        ensure!(first < n && timestamp(&times,n,first) == time, "ordered row missing");
        self.deliver_base(time, bi, first);
        Ok(true)
    }

    pub fn cell(&mut self, store: &mut Store, ci: usize) -> Result<Cell> {
        if let Some(time) = self.overlay_row {
            let row = self.overlay.as_ref().and_then(|o| o.get(&time)).and_then(Option::as_ref).context("journal row missing")?;
            return Ok(row[ci].clone());
        }
        let (bi, ri) = self.current.context("cursor is exhausted")?;
        if self.columns[ci].is_none() { self.columns[ci] = Some(store.snapshot.load(self.ti, bi, ci)?); }
        Ok(decode(self.columns[ci].as_ref().unwrap(), self.rows, ri, self.kinds[ci]))
    }
}

/// A WHERE term the virtual table evaluates itself for ORDER BY ... LIMIT.
pub(crate) struct Filter { pub column: usize, pub op: String, pub value: Value }

impl Filter {
    /// SQLite's result for this row, or None if this engine cannot decide it
    /// exactly, for example for a text operand.
    fn test(&self, cell: &Cell) -> Option<bool> {
        match self.op.as_str() {
            "null" => return Some(*cell == Cell::Null),
            "notnull" => return Some(*cell != Cell::Null),
            // IS and IS NOT compare NULL like a value.
            "is" | "isnot" => {
                let same = match (*cell == Cell::Null, self.value == Value::Null) {
                    (true, true) => true, (false, false) => compare_numeric(cell, &self.value)?.is_eq(), _ => false,
                };
                return Some(same == (self.op == "is"));
            }
            _ => (),
        }
        if *cell == Cell::Null || self.value == Value::Null { return Some(false); }
        let order = compare_numeric(cell, &self.value)?;
        Some(match self.op.as_str() {
            "eq" => order.is_eq(), "ne" => order.is_ne(), "gt" => order.is_gt(), "ge" => order.is_ge(),
            "lt" => order.is_lt(), "le" => order.is_le(), _ => return None,
        })
    }
}

/// Exact comparison of a stored number with a numeric SQL value.
fn compare_numeric(cell: &Cell, value: &Value) -> Option<std::cmp::Ordering> {
    match (cell, value) {
        (Cell::Integer(a), Value::Integer(b)) => Some(a.cmp(b)),
        (Cell::Real(a), Value::Real(b)) => f64::from_bits(*a).partial_cmp(b),
        (Cell::Real(a), Value::Integer(b)) => compare_int_real(*b, f64::from_bits(*a)).map(std::cmp::Ordering::reverse),
        (Cell::Integer(a), Value::Real(b)) => compare_int_real(*a, *b),
        _ => None,
    }
}

fn compare_int_real(a: i64, b: f64) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering;
    if b.is_nan() { return None; }
    if b < -9223372036854775808.0 { return Some(Ordering::Greater); }
    if b >= 9223372036854775808.0 { return Some(Ordering::Less); }
    let whole = b.floor() as i64;
    Some(a.cmp(&whole).then(if b > whole as f64 { Ordering::Less } else { Ordering::Equal }))
}

/// A time bound and whether it is inclusive; None is unbounded.
pub(crate) type Bound = Option<(i64, bool)>;

/// Sort key in SQLite order: NULL first, then numbers.
#[derive(Clone, Copy, PartialEq, PartialOrd)]
enum Key { Null, Number(f64), Integer(i64) }

impl Key {
    fn of(cell: &Cell) -> Self {
        match cell { Cell::Integer(n) => Key::Integer(*n), Cell::Real(n) => Key::Number(f64::from_bits(*n)), _ => Key::Null }
    }
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match (self, other) {
            (Key::Null, Key::Null) => Ordering::Equal, (Key::Null, _) => Ordering::Less, (_, Key::Null) => Ordering::Greater,
            (Key::Integer(a), Key::Integer(b)) => a.cmp(b),
            (Key::Number(a), Key::Number(b)) => a.total_cmp(b),
            (Key::Integer(a), Key::Number(b)) => compare_int_real(*a, *b).unwrap_or(Ordering::Equal),
            (Key::Number(a), Key::Integer(b)) => compare_int_real(*b, *a).unwrap_or(Ordering::Equal).reverse(),
        }
    }
    fn approximate(&self) -> Option<f64> {
        match self { Key::Null => None, Key::Number(n) => Some(*n), Key::Integer(n) => Some(*n as f64) }
    }
}

impl Store {
    /// Call `visit(time, values)` for every row within the bounds, with the
    /// values of `columns` in that order. Base rows come block by block in
    /// time order, then the journal rows; replaced base rows are skipped.
    pub(crate) fn visit_rows(&mut self, table: &str, columns: &[usize], (lower, upper): (Bound, Bound),
            mut visit: impl FnMut(i64, &[Cell]) -> Result<bool>) -> Result<()> {
        let ti = self.table_index(table)?;
        let ts = self.snapshot.manifest.tables[ti].columns.iter().position(|c| c.name == "dateTime").unwrap();
        let kinds: Vec<Kind> = columns.iter().map(|&ci| self.snapshot.manifest.tables[ti].columns[ci].kind).collect();
        let overlay = self.changes.get(&self.snapshot.manifest.tables[ti].name).filter(|o| !o.is_empty()).cloned();
        let mut values = vec![Cell::Null; columns.len()];
        let first = self.snapshot.manifest.tables[ti].blocks.partition_point(|b| before(lower, b.last));
        for bi in first..self.snapshot.manifest.tables[ti].blocks.len() {
            let block = &self.snapshot.manifest.tables[ti].blocks[bi];
            if after(upper, block.first) { break; }
            let n = block.rows;
            let times = self.snapshot.load(ti, bi, ts)?;
            let data = columns.iter().map(|&ci| self.snapshot.load(ti, bi, ci)).collect::<Result<Vec<_>>>()?;
            for ri in 0..n {
                let time = timestamp(&times, n, ri);
                if before(lower, time) { continue; }
                if after(upper, time) { break; }
                if overlay.as_ref().is_some_and(|o| o.contains_key(&time)) { continue; }
                for ((value, bytes), kind) in values.iter_mut().zip(&data).zip(&kinds) { *value = decode(bytes, n, ri, *kind); }
                if !visit(time, &values)? { return Ok(()); }
            }
        }
        if let Some(overlay) = &overlay {
            for (&time, row) in overlay.iter() {
                let Some(row) = row else { continue };
                if before(lower, time) || after(upper, time) { continue; }
                for (value, &ci) in values.iter_mut().zip(columns) { *value = row[ci].clone(); }
                if !visit(time, &values)? { return Ok(()); }
            }
        }
        Ok(())
    }

    /// Times of the rows within the bounds that pass every filter, ordered by
    /// `column` (ascending or descending, NULL lowest) and then by ascending
    /// time, truncated to `limit`. None means a filter cannot be evaluated
    /// exactly; the caller must then let SQLite sort.
    pub(crate) fn ordered_times(&mut self, table: &str, column: usize, descending: bool,
            (lower, upper): (Bound, Bound), filters: &[Filter], limit: usize) -> Result<Option<Vec<i64>>> {
        use std::cmp::Ordering;
        let ti = self.table_index(table)?;
        let ts = self.snapshot.manifest.tables[ti].columns.iter().position(|c| c.name == "dateTime").unwrap();
        let kinds: Vec<Kind> = self.snapshot.manifest.tables[ti].columns.iter().map(|c| c.kind).collect();
        let overlay = self.changes.get(&self.snapshot.manifest.tables[ti].name).cloned();
        let rank = |a: &(Key, i64), b: &(Key, i64)| -> Ordering {
            let order = a.0.cmp(&b.0);
            (if descending { order.reverse() } else { order }).then(a.1.cmp(&b.1))
        };
        let mut best: Vec<(Key, i64)> = Vec::new();
        if limit == 0 { return Ok(Some(vec![])); }
        let shrink = |best: &mut Vec<(Key, i64)>| { best.sort_by(rank); best.truncate(limit); };
        let cutoff = |best: &Vec<(Key, i64)>| -> Option<Key> { (best.len() >= limit).then(|| best[limit - 1].0) };
        let blocks = self.snapshot.manifest.tables[ti].blocks.len();
        let first = self.snapshot.manifest.tables[ti].blocks.partition_point(|b| before(lower, b.last));
        for bi in first..blocks {
            let block = &self.snapshot.manifest.tables[ti].blocks[bi];
            if after(upper, block.first) { break; }
            let n = block.rows;
            // A full candidate list makes a block irrelevant when no value in
            // it can rank above the current last candidate.
            if let Some(worst) = cutoff(&best) && let Some(stats) = self.snapshot.block_summary(ti, bi, column) {
                let complete = stats.count == n as u64;
                let skip = match (descending, worst.approximate()) {
                    // NULL ranks lowest. Later blocks also lose every tie on time.
                    (_, _) if stats.count == 0 => descending || worst == Key::Null,
                    (true, Some(w)) => stats.max.is_some_and(|max| max + max.abs() * 1e-12 < w),
                    (false, Some(w)) => complete && stats.min.is_some_and(|min| min - min.abs() * 1e-12 > w),
                    (true, None) => false, (false, None) => true,
                };
                if skip { continue; }
            }
            let times = self.snapshot.load(ti, bi, ts)?;
            let values = self.snapshot.load(ti, bi, column)?;
            let mut tested = Vec::with_capacity(filters.len());
            for filter in filters { tested.push(self.snapshot.load(ti, bi, filter.column)?); }
            for ri in 0..n {
                let time = timestamp(&times, n, ri);
                if before(lower, time) { continue; }
                if after(upper, time) { break; }
                if overlay.as_ref().is_some_and(|o| o.contains_key(&time)) { continue; }
                let mut pass = true;
                for (filter, bytes) in filters.iter().zip(&tested) {
                    match filter.test(&decode(bytes, n, ri, kinds[filter.column])) { Some(true) => (), Some(false) => { pass = false; break; }, None => return Ok(None) }
                }
                if !pass { continue; }
                best.push((Key::of(&decode(&values, n, ri, kinds[column])), time));
                if best.len() >= limit.saturating_mul(2).max(64) { shrink(&mut best); }
            }
        }
        if let Some(overlay) = &overlay {
            for (&time, row) in overlay.iter() {
                let Some(row) = row else { continue };
                if before(lower, time) || after(upper, time) { continue; }
                let mut pass = true;
                for filter in filters {
                    match filter.test(&row[filter.column]) { Some(true) => (), Some(false) => { pass = false; break; }, None => return Ok(None) }
                }
                if pass { best.push((Key::of(&row[column]), time)); }
            }
        }
        shrink(&mut best);
        Ok(Some(best.into_iter().map(|(_, time)| time).collect()))
    }
}

fn before(bound: Option<(i64,bool)>, time: i64) -> bool { bound.is_some_and(|(t,i)| time < t || (time == t && !i)) }
fn after(bound: Option<(i64,bool)>, time: i64) -> bool { bound.is_some_and(|(t,i)| time > t || (time == t && !i)) }
