//! Immutable WeeWX snapshots with independently readable numeric column blocks.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};
use rusqlite::types::{Value, ValueRef};
use rusqlite::{Connection, OpenFlags, params_from_iter};
use serde::{Deserialize, Serialize};

pub mod store;
pub mod container;
mod changes;
pub mod computed;
pub mod weather;
pub mod solar;
pub mod analytics;
pub mod window;
pub mod astro;
pub mod sql;
#[cfg(feature = "python")]
mod python;

const BLOCK_ROWS: usize = 4096;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Kind {
    Integer,
    Real,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Column {
    pub name: String,
    pub kind: Kind,
    #[serde(default)]
    pub not_null: bool,
    #[serde(default)]
    pub default: Option<Cell>,
}

pub(crate) fn numeric_cell(kind: Kind, cell: Cell) -> Cell {
    let cell = if let Cell::Text(text) = &cell {
        if let Ok(n) = text.trim().parse::<i64>() { Cell::Integer(n) }
        else if let Ok(n) = text.trim().parse::<f64>() { Cell::Real(n.to_bits()) }
        else { cell }
    } else { cell };
    match (kind,cell) {
        (Kind::Real,Cell::Integer(n)) => Cell::Real((n as f64).to_bits()),
        (Kind::Integer,Cell::Real(bits)) => {
            let n = f64::from_bits(bits);
            // WeeWX's LOOP accumulators can supply integral floating-point
            // weights for INTEGER daily-summary columns. Match numeric affinity
            // without saturating or silently rounding a non-integral value.
            if n.fract() == 0.0 && n >= i64::MIN as f64 && n < -(i64::MIN as f64) { Cell::Integer(n as i64) }
            else { Cell::Real(bits) }
        }
        (_,cell) => cell,
    }
}

/// One column of one block. `chunk` names its payload in the file; zero
/// means the column is NULL in every row and has no payload.
#[derive(Clone, Debug, PartialEq)]
struct ColumnBlock {
    chunk: u64,
    length: usize,
    checksum: u32,
    stats: Option<Stats>,
    compressed: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct Block {
    rows: usize,
    first: i64,
    last: i64,
    columns: Vec<ColumnBlock>,
}

#[derive(Clone, Debug)]
struct Table {
    /// Never reused in a file, unlike the name.
    ordinal: u64,
    name: String,
    sql: String,
    columns: Vec<Column>,
    blocks: Vec<Block>,
    rows: u64,
}

// JSON numbers cannot preserve every SQLite integer/REAL distinction. Store
// REAL bits explicitly, including negative zero, for a lossless export.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub enum Cell {
    Null,
    Integer(i64),
    Real(u64),
    Text(String),
    Blob(Vec<u8>),
}

impl Cell {
    pub fn read(value: ValueRef<'_>) -> Result<Self> {
        Ok(match value {
            ValueRef::Null => Self::Null,
            ValueRef::Integer(n) => Self::Integer(n),
            ValueRef::Real(n) => Self::Real(n.to_bits()),
            ValueRef::Text(s) => Self::Text(std::str::from_utf8(s)?.to_owned()),
            ValueRef::Blob(b) => Self::Blob(b.to_vec()),
        })
    }

    pub fn value(&self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Integer(n) => Value::Integer(*n),
            Self::Real(bits) => Value::Real(f64::from_bits(*bits)),
            Self::Text(s) => Value::Text(s.clone()),
            Self::Blob(b) => Value::Blob(b.clone()),
        }
    }

    pub fn numeric(&self) -> Option<f64> {
        match self {
            Self::Integer(n) => Some(*n as f64),
            Self::Real(bits) => Some(f64::from_bits(*bits)),
            _ => None,
        }
    }

    pub fn from_json(value: &serde_json::Value) -> Result<Self> {
        Ok(match value {
            serde_json::Value::Null => Self::Null,
            serde_json::Value::Number(n) if n.is_i64() => Self::Integer(n.as_i64().unwrap()),
            serde_json::Value::Number(n) => Self::Real(n.as_f64().context("invalid number")?.to_bits()),
            serde_json::Value::String(s) => Self::Text(s.clone()),
            _ => bail!("expected a SQL scalar value"),
        })
    }

    pub fn json(&self) -> serde_json::Value {
        match self {
            Self::Null => serde_json::Value::Null,
            Self::Integer(n) => (*n).into(),
            Self::Real(bits) => serde_json::json!(f64::from_bits(*bits)),
            Self::Text(s) => s.clone().into(),
            Self::Blob(b) => serde_json::json!({"blob": b}),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct MetadataTable {
    name: String,
    sql: String,
    rows: Vec<Vec<Cell>>,
}

/// Schemas and the block index of one snapshot. The tables are shared
/// between connections and only copied when a schema changes.
#[derive(Clone, Debug)]
struct Manifest {
    archive: String,
    tables: Arc<Vec<Table>>,
    metadata: Vec<MetadataTable>,
    /// The database's identity, fixed at creation.
    id: String,
    /// The last transaction this snapshot includes.
    lsn: u64,
    /// The block index and schemas, which a compaction or schema change replace.
    generation: u64,
}

/// SQL aggregate semantics: an empty or all-NULL input has a NULL sum.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Stats {
    pub count: u64,
    pub sum: Option<f64>,
    pub min: Option<f64>,
    pub max: Option<f64>,
}

#[derive(Clone, Default)]
struct Accumulator {
    stats: Stats,
    correction: f64,
}

impl Accumulator {
    fn merge(&mut self, stats: &Stats) {
        if stats.count == 0 { return; }
        let old_count = self.stats.count;
        let old_min = self.stats.min;
        let old_max = self.stats.max;
        self.add(stats.sum);
        self.stats.count = old_count + stats.count;
        self.stats.min = match (old_min, stats.min) {
            (Some(a), Some(b)) => Some(a.min(b)), (a, b) => a.or(b),
        };
        self.stats.max = match (old_max, stats.max) {
            (Some(a), Some(b)) => Some(a.max(b)), (a, b) => a.or(b),
        };
    }
    fn add(&mut self, value: Option<f64>) {
        if let Some(value) = value {
            self.stats.count += 1;
            // Compensate rounding when temperatures or other signed readings
            // cancel. A plain sum loses 1.0 in [1e16, 1.0, -1e16]. An infinite
            // reading makes the sum infinite, and its correction would be NaN.
            let previous = self.stats.sum.unwrap_or(0.0);
            let sum = previous + value;
            if sum.is_finite() {
                self.correction += if previous.abs() >= value.abs() {
                    (previous - sum) + value
                } else {
                    (value - sum) + previous
                };
            }
            self.stats.sum = Some(sum);
            self.stats.min = Some(self.stats.min.map_or(value, |n| n.min(value)));
            self.stats.max = Some(self.stats.max.map_or(value, |n| n.max(value)));
        }
    }

    fn finish(mut self) -> Stats {
        // As in SQLite, +inf and -inf together sum to NULL.
        self.stats.sum = self.stats.sum.map(|sum| if sum.is_finite() { sum + self.correction } else { sum }).filter(|sum| !sum.is_nan());
        self.stats
    }
}

/// Archive records use (start, stop]; daily summaries use [start, stop).
#[derive(Clone, Copy)]
pub struct Span {
    pub start: i64,
    pub stop: i64,
    pub daily: bool,
}

impl Span {
    fn before(&self, ts: i64) -> bool {
        if self.daily {
            ts < self.start
        } else {
            ts <= self.start
        }
    }

    fn after(&self, ts: i64) -> bool {
        if self.daily {
            ts >= self.stop
        } else {
            ts > self.stop
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Info {
    pub archive: String,
    pub rows: u64,
    pub first: Option<i64>,
    pub last: Option<i64>,
    pub tables: Vec<String>,
    pub columns: Vec<String>,
    pub metadata_tables: Vec<String>,
    /// Rows and blocks of each numeric table, without uncompacted changes.
    pub blocks: std::collections::BTreeMap<String, (u64, usize)>,
    pub cache_bytes: usize,
    pub cache_limit_bytes: usize,
    pub reads: ReadMetrics,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ReadMetrics {
    pub column_reads: u64,
    pub bytes_read: u64,
    pub synopsis_hits: u64,
    pub skipped_blocks: u64,
}

/// The cache limit covers column payloads, not the index, results or OS cache.
pub struct Database {
    container: container::Container,
    manifest: Arc<Manifest>,
    cache: Arc<std::sync::Mutex<BlockCache>>,
    cache_limit: usize,
    metrics: ReadMetrics,
}

/// Decoded column payloads by chunk id with the CRC of their stored bytes,
/// oldest first out. A chunk id names fixed content, except where a rolled
/// back transaction had allocated it before; the CRC tells those apart.
struct BlockCache { blocks: HashMap<u64, (u32, Arc<[u8]>)>, order: VecDeque<u64>, bytes: usize, limit: usize }

/// Indices and decoded blocks of one database, shared by every connection of
/// this process. WeeWX opens new connections for each report, which then
/// start with a parsed index and warm blocks. Only committed state is shared.
struct SharedFile { id: String, indices: Vec<(u64, Arc<Vec<Table>>)>, cache: Arc<std::sync::Mutex<BlockCache>> }

fn shared_files() -> &'static std::sync::Mutex<HashMap<std::path::PathBuf, SharedFile>> {
    static FILES: std::sync::OnceLock<std::sync::Mutex<HashMap<std::path::PathBuf, SharedFile>>> = std::sync::OnceLock::new();
    FILES.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Indices kept per database: the current and the previous generation.
const SHARED_INDICES: usize = 2;

/// The text of a caught panic.
pub(crate) fn panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    panic.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "panic".to_owned())
}

pub fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

pub fn open_sqlite(path: &Path, cache_bytes: usize) -> Result<Connection> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.pragma_update(None, "cache_size", -((cache_bytes / 1024) as i64))?;
    connection.pragma_update(None, "mmap_size", 0)?;
    Ok(connection)
}

fn column_kind(declared: &str) -> Result<Kind> {
    // SQLite's affinity rules: INT means INTEGER; REAL, FLOA and DOUB mean
    // REAL. NUMERIC, DECIMAL, BOOLEAN and undeclared types hold numbers in a
    // WeeWX archive and are stored as REAL. Text columns cannot be stored.
    let declared = declared.to_ascii_uppercase();
    if declared.contains("INT") {
        Ok(Kind::Integer)
    } else if ["CHAR", "CLOB", "TEXT", "BLOB"].iter().any(|s| declared.contains(s)) {
        bail!("unsupported column type {declared:?}; only numeric columns can be stored")
    } else {
        Ok(Kind::Real)
    }
}

fn block_bytes(rows: usize) -> usize {
    rows.div_ceil(8) + rows * 8
}

fn rows_per_block(table: &str) -> usize { if table.contains("_day_") { 128 } else { BLOCK_ROWS } }

/// The stored payload of one column of a block: a NULL bitmap, eight bytes
/// per row and, for an INTEGER column holding REAL values as SQLite allows, a
/// bitmap of the REAL rows. An all-NULL column has no payload.
fn encode_cells(kind: Kind, cells: &[&Cell]) -> Result<Vec<u8>> {
    if cells.iter().all(|c| matches!(c, Cell::Null)) { return Ok(Vec::new()); }
    let rows = cells.len();
    let bitmap_bytes = rows.div_ceil(8);
    let mixed = kind == Kind::Integer && cells.iter().any(|c| matches!(c, Cell::Real(_)));
    let mut bytes = vec![0; block_bytes(rows) + if mixed { bitmap_bytes } else { 0 }];
    for (i, cell) in cells.iter().enumerate() {
        let value = match (kind, cell) {
            (_, Cell::Null) => continue,
            (Kind::Integer, Cell::Integer(n)) => *n as u64,
            (Kind::Real, Cell::Real(bits)) => *bits,
            (Kind::Integer, Cell::Real(bits)) => { bytes[block_bytes(rows) + i / 8] |= 1 << (i % 8); *bits }
            (Kind::Real, Cell::Integer(n)) => (*n as f64).to_bits(),
            _ => bail!("text or blob in a numeric column"),
        };
        bytes[i / 8] |= 1 << (i % 8);
        bytes[bitmap_bytes + i * 8..bitmap_bytes + (i + 1) * 8].copy_from_slice(&value.to_le_bytes());
    }
    Ok(bytes)
}

fn encode_full(values: &[Option<u64>]) -> Vec<u8> {
    let bitmap_bytes = values.len().div_ceil(8);
    let mut bytes = vec![0; block_bytes(values.len())];
    for (i, value) in values.iter().enumerate() {
        if let Some(value) = value {
            bytes[i / 8] |= 1 << (i % 8);
            bytes[bitmap_bytes + i * 8..bitmap_bytes + (i + 1) * 8]
                .copy_from_slice(&value.to_le_bytes());
        }
    }
    bytes
}

fn stored_column(raw: Vec<u8>) -> (Vec<u8>,bool) {
    if raw.is_empty() { return (raw,false); }
    let compressed = lz4_flex::block::compress(&raw);
    if compressed.len()+16 < raw.len() { (compressed,true) } else { (raw,false) }
}

/// Whether the REAL bitmap marks row `i` of an INTEGER column.
fn real_row(bytes: &[u8], rows: usize, i: usize) -> bool {
    bytes.len() > block_bytes(rows) && bytes[block_bytes(rows) + i / 8] & (1 << (i % 8)) != 0
}

/// The cell at row `i` of a decoded column payload.
fn cell_at(bytes: &[u8], rows: usize, i: usize, kind: Kind) -> Cell {
    match bits(bytes, rows, i) {
        None => Cell::Null,
        Some(n) if kind == Kind::Real || real_row(bytes, rows, i) => Cell::Real(n),
        Some(n) => Cell::Integer(n as i64),
    }
}

/// An empty block stands for a column whose rows are all NULL.
fn bits(bytes: &[u8], rows: usize, i: usize) -> Option<u64> {
    if bytes.is_empty() || bytes[i / 8] & (1 << (i % 8)) == 0 {
        return None;
    }
    let start = rows.div_ceil(8) + i * 8;
    Some(u64::from_le_bytes(
        bytes[start..start + 8].try_into().unwrap(),
    ))
}

fn number(bytes: &[u8], rows: usize, i: usize, kind: Kind) -> Option<f64> {
    bits(bytes, rows, i).map(|value| match kind {
        Kind::Integer if !real_row(bytes, rows, i) => (value as i64) as f64,
        _ => f64::from_bits(value),
    })
}

fn timestamp(bytes: &[u8], rows: usize, i: usize) -> i64 {
    bits(bytes, rows, i).unwrap() as i64
}

fn null_column() -> Arc<[u8]> {
    static EMPTY: std::sync::OnceLock<Arc<[u8]>> = std::sync::OnceLock::new();
    EMPTY.get_or_init(|| Arc::from(Vec::new())).clone()
}

fn new_cache() -> Arc<std::sync::Mutex<BlockCache>> {
    Arc::new(std::sync::Mutex::new(BlockCache { blocks: HashMap::new(), order: VecDeque::new(), bytes: 0, limit: 0 }))
}

fn new_id() -> Result<String> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("random database id: {e}"))?;
    Ok(format!("{:032x}", u128::from_le_bytes(bytes)))
}

/// Import a consistent read snapshot without modifying or replacing the source.
/// The archive table, its daily-summary tables and their metadata are copied.
pub fn import_sqlite(source: &Path, destination: &Path, archive: &str) -> Result<()> {
    let mut source = open_sqlite(source, 1024 * 1024)?;
    let transaction = source.transaction()?;
    let definitions = transaction
        .prepare("SELECT name, sql FROM sqlite_master WHERE type='table' ORDER BY name")?
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(definitions.iter().any(|(name, _)| name == archive), "archive table {archive:?} missing");
    container::Container::create(destination, archive, |output| {
        let mut next_table = 1;
        let mut next_chunk = 1;
        for (name, sql) in definitions {
            if name != archive && !name.starts_with(&format!("{archive}_day_")) {
                continue;
            }
            if name == format!("{archive}_day__metadata") {
                let mut statement = transaction.prepare(&format!("SELECT * FROM {}", quote(&name)))?;
                let count = statement.column_count();
                let mut rows = statement.query([])?;
                let mut data = Vec::new();
                while let Some(row) = rows.next()? {
                    data.push((0..count).map(|i| Cell::read(row.get_ref(i)?)).collect::<Result<Vec<_>>>()?);
                    ensure!(data.len() <= 4096, "metadata table {name} has more than 4096 rows");
                }
                output.write_metadata(&MetadataTable { name, sql, rows: data })?;
                continue;
            }
            let columns = transaction
                .prepare(&format!("PRAGMA table_info({})", quote(&name)))?
                .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, bool>(3)?, r.get::<_, Option<String>>(4)?)))?
                .map(|r| {
                    let (column, declared, not_null, default) = r?;
                    let kind = column_kind(&declared).with_context(|| format!("{name}.{column}"))?;
                    let default = default.map(|value| transaction.query_row(&format!("SELECT {value}"), [], |r| r.get::<_, Value>(0))).transpose()?;
                    let default = default.map(|value| Cell::read(ValueRef::from(&value))).transpose()?.map(|c| numeric_cell(kind, c));
                    Ok(Column { not_null: not_null || column == "dateTime", default, name: column, kind })
                })
                .collect::<Result<Vec<_>>>()?;
            let time_index = columns.iter().position(|c| c.name == "dateTime").with_context(|| format!("{name} has no dateTime column"))?;
            ensure!(matches!(columns[time_index].kind, Kind::Integer), "{name}.dateTime must be INTEGER");
            let mut table = Table { ordinal: next_table, name, sql, columns, blocks: vec![], rows: 0 };
            next_table += 1;
            let mut select = transaction.prepare(&format!("SELECT * FROM {} ORDER BY dateTime", quote(&table.name)))?;
            let mut rows = select.query([])?;
            let mut previous = None;
            let limit = rows_per_block(&table.name);
            loop {
                let mut batch = Vec::with_capacity(limit);
                while batch.len() < limit {
                    let Some(row) = rows.next()? else { break };
                    let at = row.get_ref(time_index)?;
                    let mut cells = Vec::with_capacity(table.columns.len());
                    for (i, column) in table.columns.iter().enumerate() {
                        let cell = numeric_cell(column.kind, Cell::read(row.get_ref(i)?)?);
                        store::validate_cell(column, &cell).with_context(|| format!("{} at dateTime {at:?}", table.name))?;
                        cells.push(cell);
                    }
                    let Cell::Integer(time) = cells[time_index] else { bail!("{}.dateTime must be an integer", table.name) };
                    ensure!(previous.is_none_or(|p| time > p), "timestamps must be unique and increasing in {}", table.name);
                    previous = Some(time);
                    batch.push(cells);
                }
                if batch.is_empty() { break; }
                store::write_block(output, &mut next_chunk, &table.columns, &batch, &mut table.blocks)?;
            }
            table.rows = table.blocks.iter().map(|b| b.rows as u64).sum();
            output.write_table(&table)?;
            output.write_blocks(table.ordinal, &[], &table.blocks)?;
        }
        output.set_counter("next_table", next_table)?;
        output.set_counter("next_chunk", next_chunk)?;
        Ok(())
    })
}

impl Database {
    /// Open a database for reading only, without its uncompacted changes.
    pub fn open(path: &Path, cache_limit: usize) -> Result<Self> {
        let container = container::Container::open(path, std::time::Duration::from_secs(5), true)?;
        let mut database = Self::new(container, cache_limit);
        database.container.begin_read()?;
        let loaded = database.container.meta().and_then(|meta| database.load_index(&meta, true));
        database.container.rollback();
        loaded?;
        Ok(database)
    }

    pub(crate) fn new(container: container::Container, cache_limit: usize) -> Self {
        let manifest = Arc::new(Manifest { archive: String::new(), tables: Arc::new(vec![]), metadata: vec![], id: String::new(), lsn: 0, generation: 0 });
        Self { container, manifest, cache: new_cache(), cache_limit, metrics: ReadMetrics::default() }
    }

    /// Read the block index and metadata tables of `meta`. With `committed`,
    /// the process shares the index: only state read outside a write
    /// transaction is known to be committed.
    pub(crate) fn load_index(&mut self, meta: &container::Meta, committed: bool) -> Result<()> {
        let path = self.container.path.clone();
        let mut tables = None;
        if committed {
            let mut files = shared_files().lock().map_err(|_| anyhow::anyhow!("database registry poisoned"))?;
            let shared = files.entry(path.clone()).or_insert_with(|| SharedFile { id: meta.uuid.clone(), indices: vec![], cache: new_cache() });
            // Another database now lives at this path.
            if shared.id != meta.uuid { *shared = SharedFile { id: meta.uuid.clone(), indices: vec![], cache: new_cache() }; }
            self.cache = shared.cache.clone();
            tables = shared.indices.iter().find(|(generation, _)| *generation == meta.generation).map(|(_, tables)| tables.clone());
        }
        let tables = match tables {
            Some(tables) => tables,
            None => {
                let loaded = Arc::new(self.container.load_tables()?);
                if committed && let Ok(mut files) = shared_files().lock() && let Some(shared) = files.get_mut(&path).filter(|s| s.id == meta.uuid) {
                    shared.indices.retain(|(generation, _)| *generation != meta.generation);
                    if shared.indices.len() >= SHARED_INDICES { shared.indices.remove(0); }
                    shared.indices.push((meta.generation, loaded.clone()));
                }
                loaded
            }
        };
        {
            let mut cache = self.cache.lock().map_err(|_| anyhow::anyhow!("block cache poisoned"))?;
            cache.limit = cache.limit.max(self.cache_limit);
        }
        let metadata = self.container.load_metadata()?;
        self.manifest = Arc::new(Manifest { archive: meta.archive.clone(), tables, metadata, id: meta.uuid.clone(), lsn: meta.seq, generation: meta.generation });
        Ok(())
    }

    pub fn info(&self) -> Info {
        let table = self.manifest.tables.iter().find(|t| t.name == self.manifest.archive);
        Info {
            archive: self.manifest.archive.clone(),
            rows: table.map_or(0, |t| t.rows),
            first: table.and_then(|t| t.blocks.first().map(|b| b.first)),
            last: table.and_then(|t| t.blocks.last().map(|b| b.last)),
            tables: self
                .manifest
                .tables
                .iter()
                .map(|t| t.name.clone())
                .collect(),
            columns: table.map_or_else(Vec::new, |t| t.columns.iter().map(|c| c.name.clone()).collect()),
            metadata_tables: self
                .manifest
                .metadata
                .iter()
                .map(|t| t.name.clone())
                .collect(),
            blocks: self.manifest.tables.iter().map(|t| (t.name.clone(), (t.rows, t.blocks.len()))).collect(),
            cache_bytes: self.cache.lock().map_or(0, |c| c.bytes),
            cache_limit_bytes: self.cache.lock().map_or(self.cache_limit, |c| c.limit),
            reads: self.metrics.clone(),
        }
    }

    fn table_column(&self, table: &str, column: &str) -> Result<(usize, usize, usize)> {
        let ti = self
            .manifest
            .tables
            .iter()
            .position(|t| t.name.eq_ignore_ascii_case(table))
            .context("unknown table")?;
        let columns = &self.manifest.tables[ti].columns;
        let ci = columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(column))
            .context("unknown column")?;
        let ts = columns.iter().position(|c| c.name == "dateTime").unwrap();
        Ok((ti, ci, ts))
    }

    fn load(&mut self, ti: usize, bi: usize, ci: usize) -> Result<Arc<[u8]>> {
        let block = &self.manifest.tables[ti].blocks[bi];
        if ci >= block.columns.len() {
            let default = self.manifest.tables[ti].columns[ci].default.as_ref().and_then(|c| match c {
                Cell::Integer(n) => Some(*n as u64), Cell::Real(n) => Some(*n), _ => None,
            });
            if default.is_none() { return Ok(null_column()); }
            return Ok(encode_full(&vec![default; block.rows]).into());
        }
        let column = &block.columns[ci];
        if column.length == 0 {
            return Ok(null_column());
        }
        if let Some((checksum, bytes)) = self.cache.lock().map_err(|_| anyhow::anyhow!("block cache poisoned"))?.blocks.get(&column.chunk)
            && *checksum == column.checksum {
            return Ok(bytes.clone());
        }
        let expected = block_bytes(block.rows);
        // INTEGER columns may carry a bitmap of REAL rows after the values.
        let mixed = self.manifest.tables[ti].columns[ci].kind == Kind::Integer;
        let largest = expected + if mixed { block.rows.div_ceil(8) } else { 0 };
        let (chunk, length, checksum, compressed) = (column.chunk, column.length, column.checksum, column.compressed);
        let mut bytes = self.container.read_chunk(chunk)?;
        self.metrics.column_reads += 1;
        self.metrics.bytes_read += bytes.len() as u64;
        ensure!(bytes.len() == length && crc32fast::hash(&bytes) == checksum, "column chunk {chunk} checksum mismatch");
        if compressed {
            bytes = lz4_flex::block::decompress(&bytes, largest).with_context(|| format!("invalid compressed column chunk {chunk}"))?;
        }
        ensure!(bytes.len() == expected || bytes.len() == largest, "invalid length of column chunk {chunk}");
        let bytes: Arc<[u8]> = bytes.into();
        let mut cache = self.cache.lock().map_err(|_| anyhow::anyhow!("block cache poisoned"))?;
        if bytes.len() <= cache.limit {
            if let Some((_, old)) = cache.blocks.remove(&chunk) {
                cache.bytes -= old.len();
                cache.order.retain(|&id| id != chunk);
            }
            while cache.bytes + bytes.len() > cache.limit {
                let oldest = cache.order.pop_front().unwrap();
                let removed = cache.blocks.remove(&oldest).unwrap().1.len();
                cache.bytes -= removed;
            }
            cache.bytes += bytes.len();
            cache.order.push_back(chunk);
            cache.blocks.insert(chunk, (checksum, bytes.clone()));
        }
        Ok(bytes)
    }

    fn scan(
        &mut self,
        table: &str,
        column: &str,
        span: Span,
        mut visit: impl FnMut(i64, Option<f64>),
    ) -> Result<()> {
        ensure!(span.start <= span.stop, "reversed time span");
        let (ti, ci, ts) = self.table_column(table, column)?;
        let kind = self.manifest.tables[ti].columns[ci].kind;
        let blocks = &self.manifest.tables[ti].blocks;
        let first = blocks.partition_point(|b| span.before(b.last));
        let stop = blocks.partition_point(|b| !span.after(b.first));
        for bi in first..stop {
            if self.manifest.tables[ti].blocks[bi].columns.get(ci).is_some_and(|c| c.length == 0) {
                continue;
            }
            let n = self.manifest.tables[ti].blocks[bi].rows;
            let times = self.load(ti, bi, ts)?;
            // Timestamps participate in binary search, so a NULL timestamp must
            // be rejected even when the affected measurement itself is NULL.
            // A block summary counting every row already proves that.
            let summarized = self.manifest.tables[ti].blocks[bi].columns.get(ts)
                .and_then(|c| c.stats.as_ref()).is_some_and(|s| s.count == n as u64);
            ensure!(
                summarized || (0..n).all(|i| bits(&times, n, i).is_some()),
                "NULL timestamp"
            );
            let mut lower = 0;
            let mut upper = n;
            while lower < upper {
                let middle = lower + (upper - lower) / 2;
                if span.before(timestamp(&times, n, middle)) {
                    lower = middle + 1;
                } else {
                    upper = middle;
                }
            }
            let values = self.load(ti, bi, ci)?;
            for i in lower..n {
                let time = timestamp(&times, n, i);
                if span.after(time) {
                    break;
                }
                visit(time, number(&values, n, i, kind));
            }
        }
        Ok(())
    }

    pub fn aggregate(&mut self, table: &str, column: &str, span: Span) -> Result<Stats> {
        self.aggregate_excluding::<()>(table, column, span, None)
    }

    /// Rows whose timestamps are keys of `excluded` are left out; the journal
    /// supplies their current values.
    fn aggregate_excluding<V>(&mut self, table: &str, column: &str, span: Span, excluded: Option<&std::collections::BTreeMap<i64, V>>) -> Result<Stats> {
        ensure!(span.start <= span.stop, "reversed time span");
        let (ti, ci, ts) = self.table_column(table, column)?;
        let mut answer = Accumulator::default();
        let excluded = excluded.filter(|e| !e.is_empty());
        let blocks = &self.manifest.tables[ti].blocks;
        let first = blocks.partition_point(|b| span.before(b.last));
        let stop = blocks.partition_point(|b| !span.after(b.first));
        for bi in first..stop {
            let block = &self.manifest.tables[ti].blocks[bi];
            if !span.before(block.first) && !span.after(block.last)
                && excluded.is_none_or(|e| e.range(block.first..=block.last).next().is_none())
                && let Some(stats) = self.block_summary(ti,bi,ci) {
                answer.merge(&stats);
                self.metrics.synopsis_hits += 1;
                continue;
            }
            let n = block.rows;
            let kind = self.manifest.tables[ti].columns[ci].kind;
            let times = self.load(ti, bi, ts)?;
            let values = self.load(ti, bi, ci)?;
            for i in 0..n {
                let time = timestamp(&times, n, i);
                if !span.before(time) && !span.after(time) && !excluded.is_some_and(|e| e.contains_key(&time)) {
                    answer.add(number(&values, n, i, kind));
                }
            }
        }
        Ok(answer.finish())
    }

    fn block_summary(&self, ti: usize, bi: usize, ci: usize) -> Option<Stats> {
        let table = &self.manifest.tables[ti];
        let block = &table.blocks[bi];
        if let Some(column) = block.columns.get(ci) { return column.stats.clone(); }
        let value = table.columns[ci].default.as_ref().and_then(Cell::numeric);
        Some(match value { None => Stats::default(), Some(value) => Stats { count:block.rows as u64,
            sum:Some(value*block.rows as f64), min:Some(value), max:Some(value) } })
    }

    /// Return every (start + n * seconds, start + (n + 1) * seconds] bin,
    /// including empty bins. Callers supply epoch boundaries, including DST.
    pub fn buckets(
        &mut self,
        table: &str,
        column: &str,
        span: Span,
        seconds: i64,
    ) -> Result<Vec<Stats>> {
        ensure!(
            !span.daily && seconds > 0 && span.start <= span.stop,
            "invalid bucket span"
        );
        let length = span.stop as i128 - span.start as i128;
        let count = (length + seconds as i128 - 1) / seconds as i128;
        ensure!(count <= 1_000_000, "too many result buckets");
        let mut result = vec![Accumulator::default(); count as usize];
        self.scan(table, column, span, |time, value| {
            let index = ((time as i128 - span.start as i128 - 1) / seconds as i128) as usize;
            result[index].add(value);
        })?;
        Ok(result.into_iter().map(Accumulator::finish).collect())
    }

    pub fn daily_mean(&mut self, table: &str, start: i64, stop: i64) -> Result<Option<f64>> {
        let span = Span {
            start,
            stop,
            daily: true,
        };
        let numerator = self.aggregate(table, "wsum", span)?.sum;
        let denominator = self.aggregate(table, "sumtime", span)?.sum;
        Ok(numerator
            .zip(denominator)
            .and_then(|(a, b)| (b != 0.0).then_some(a / b)))
    }

    /// Restore imported tables, values and CREATE TABLE definitions to SQLite.
    /// User indices, triggers, views and unrelated tables are outside v0 scope.
    pub fn export_sqlite(&mut self, destination: &Path) -> Result<()> {
        let parent = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let output = tempfile::NamedTempFile::new_in(parent)?;
        {
            let mut connection = Connection::open(output.path())?;
            let transaction = connection.transaction()?;
            for ti in 0..self.manifest.tables.len() {
                let table = &self.manifest.tables[ti];
                transaction.execute_batch(&table.sql)?;
                let placeholders = vec!["?"; table.columns.len()].join(",");
                let mut insert = transaction.prepare(&format!(
                    "INSERT INTO {} VALUES ({placeholders})",
                    quote(&table.name)
                ))?;
                let block_count = table.blocks.len();
                for bi in 0..block_count {
                    let n = self.manifest.tables[ti].blocks[bi].rows;
                    let columns = (0..self.manifest.tables[ti].columns.len())
                        .map(|ci| self.load(ti, bi, ci))
                        .collect::<Result<Vec<_>>>()?;
                    for i in 0..n {
                        let values =
                            columns
                                .iter()
                                .enumerate()
                                .map(|(ci, bytes)| cell_at(bytes, n, i, self.manifest.tables[ti].columns[ci].kind).value());
                        insert.execute(params_from_iter(values))?;
                    }
                }
            }
            for table in &self.manifest.metadata {
                transaction.execute_batch(&table.sql)?;
                for row in &table.rows {
                    let placeholders = vec!["?"; row.len()].join(",");
                    transaction.execute(
                        &format!("INSERT INTO {} VALUES ({placeholders})", quote(&table.name)),
                        params_from_iter(row.iter().map(Cell::value)),
                    )?;
                }
            }
            transaction.commit()?;
        }
        publish_file(output, destination)
    }
}

/// Give a finished temporary file the permissions of a new file, synchronize
/// it and move it to `destination`, which must not exist.
pub(crate) fn publish_file(output: tempfile::NamedTempFile, destination: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // tempfile creates mode 0600; other tools expect what SQLite creates.
        let mask = unsafe { libc::umask(0o022) };
        unsafe { libc::umask(mask) };
        output.as_file().set_permissions(std::fs::Permissions::from_mode(0o644 & !(mask as u32)))?;
    }
    output.as_file().sync_all()?;
    output.persist_noclobber(destination).with_context(|| format!("destination must not already exist: {}", destination.display()))?;
    container::sync_directory(destination)
}
