//! Immutable WeeWX snapshots with independently readable numeric column blocks.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};
use rusqlite::types::{Value, ValueRef};
use rusqlite::{Connection, OpenFlags, params_from_iter};
use serde::{Deserialize, Serialize};

pub mod store;
pub mod computed;
pub mod weather;
pub mod solar;
pub mod analytics;
pub mod window;
pub mod astro;
pub mod sql;
#[cfg(feature = "python")]
mod python;

const MAGIC: &[u8; 8] = b"WWXDB002";
const HEADER_BYTES: u64 = 28;
const BLOCK_ROWS: usize = 4096;
const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;

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

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ColumnBlock {
    offset: u64,
    length: usize,
    checksum: u32,
    #[serde(default)]
    stats: Option<Stats>,
    #[serde(default)]
    compressed: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Block {
    rows: usize,
    first: i64,
    last: i64,
    columns: Vec<ColumnBlock>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Table {
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

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Manifest {
    archive: String,
    tables: Vec<Table>,
    metadata: Vec<MetadataTable>,
    #[serde(default)]
    id: String,
    #[serde(default)]
    lsn: u64,
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
            // cancel. A plain sum loses 1.0 in [1e16, 1.0, -1e16].
            let previous = self.stats.sum.unwrap_or(0.0);
            let sum = previous + value;
            self.correction += if previous.abs() >= value.abs() {
                (previous - sum) + value
            } else {
                (value - sum) + previous
            };
            self.stats.sum = Some(sum);
            self.stats.min = Some(self.stats.min.map_or(value, |n| n.min(value)));
            self.stats.max = Some(self.stats.max.map_or(value, |n| n.max(value)));
        }
    }

    fn finish(mut self) -> Stats {
        self.stats.sum = self.stats.sum.map(|sum| sum + self.correction);
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
    file: File,
    manifest: Manifest,
    cache: HashMap<u64, Arc<[u8]>>,
    order: VecDeque<u64>,
    cache_bytes: usize,
    cache_limit: usize,
    metrics: ReadMetrics,
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
    let declared = declared.to_ascii_uppercase();
    if declared.contains("INT") {
        Ok(Kind::Integer)
    } else if ["REAL", "FLOA", "DOUB"]
        .iter()
        .any(|s| declared.contains(s))
    {
        Ok(Kind::Real)
    } else {
        bail!("unsupported numeric column type {declared:?}")
    }
}

fn block_bytes(rows: usize) -> usize {
    rows.div_ceil(8) + rows * 8
}

fn rows_per_block(table: &str) -> usize { if table.contains("_day_") { 128 } else { BLOCK_ROWS } }

fn encode(values: &[Option<u64>]) -> Vec<u8> {
    if values.iter().all(Option::is_none) {
        return Vec::new();
    }
    encode_full(values)
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

fn stored_column(values: &[Option<u64>]) -> (Vec<u8>,bool) {
    let raw = encode(values);
    if raw.is_empty() { return (raw,false); }
    let compressed = lz4_flex::block::compress(&raw);
    if compressed.len()+16 < raw.len() { (compressed,true) } else { (raw,false) }
}

fn bits(bytes: &[u8], rows: usize, i: usize) -> Option<u64> {
    if bytes[i / 8] & (1 << (i % 8)) == 0 {
        return None;
    }
    let start = rows.div_ceil(8) + i * 8;
    Some(u64::from_le_bytes(
        bytes[start..start + 8].try_into().unwrap(),
    ))
}

fn number(bytes: &[u8], rows: usize, i: usize, kind: Kind) -> Option<f64> {
    bits(bytes, rows, i).map(|value| match kind {
        Kind::Integer => (value as i64) as f64,
        Kind::Real => f64::from_bits(value),
    })
}

fn timestamp(bytes: &[u8], rows: usize, i: usize) -> i64 {
    bits(bytes, rows, i).unwrap() as i64
}

fn new_id() -> Result<String> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("random database id: {e}"))?;
    Ok(format!("{:032x}", u128::from_le_bytes(bytes)))
}

/// Import a consistent read snapshot without modifying or replacing the source.
/// Only archive and its *_day_* tables belong to this experimental format.
pub fn import_sqlite(source: &Path, destination: &Path, archive: &str) -> Result<()> {
    let mut source = open_sqlite(source, 1024 * 1024)?;
    let transaction = source.transaction()?;
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut output = tempfile::NamedTempFile::new_in(parent)?;
    output.write_all(&[0; HEADER_BYTES as usize])?;
    let mut manifest = Manifest {
        archive: archive.to_owned(),
        tables: vec![],
        metadata: vec![],
        id: new_id()?,
        lsn: 0,
    };
    let mut schema = transaction
        .prepare("SELECT name, sql FROM sqlite_master WHERE type='table' ORDER BY name")?;
    let definitions = schema
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(
        definitions.iter().any(|(name, _)| name == archive),
        "archive table {archive:?} missing"
    );
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
                data.push(
                    (0..count)
                        .map(|i| Cell::read(row.get_ref(i)?))
                        .collect::<Result<Vec<_>>>()?,
                );
                ensure!(
                    data.len() <= 4096,
                    "metadata table is too large for this prototype"
                );
            }
            manifest.metadata.push(MetadataTable {
                name,
                sql,
                rows: data,
            });
            continue;
        }
        let mut pragma = transaction.prepare(&format!("PRAGMA table_info({})", quote(&name)))?;
        let columns = pragma
            .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, bool>(3)?,r.get::<_,Option<String>>(4)?)))?
            .map(|r| {
                let (name, declared, not_null, default) = r?;
                let kind = column_kind(&declared)?;
                let default = default.map(|value| transaction.query_row(&format!("SELECT {value}"),[],|r|r.get::<_,Value>(0))).transpose()?;
                let default = default.map(|value|Cell::read(ValueRef::from(&value))).transpose()?.map(|c|numeric_cell(kind,c));
                Ok(Column { not_null: not_null || name == "dateTime", default, name, kind })
            })
            .collect::<Result<Vec<_>>>()?;
        let time_index = columns
            .iter()
            .position(|c| c.name == "dateTime")
            .context("dateTime missing")?;
        ensure!(
            matches!(columns[time_index].kind, Kind::Integer),
            "dateTime must be INTEGER"
        );
        let mut table = Table {
            name,
            sql,
            columns,
            blocks: vec![],
            rows: 0,
        };
        let mut select = transaction.prepare(&format!(
            "SELECT * FROM {} ORDER BY dateTime",
            quote(&table.name)
        ))?;
        let mut rows = select.query([])?;
        let mut previous = None;
        loop {
            let limit = rows_per_block(&table.name);
            let mut values = vec![Vec::with_capacity(limit); table.columns.len()];
            let mut times = Vec::with_capacity(limit);
            while times.len() < limit {
                let Some(row) = rows.next()? else { break };
                let ts = row.get::<_, i64>(time_index)?;
                ensure!(
                    previous.is_none_or(|p| ts > p),
                    "timestamps must be unique and increasing"
                );
                previous = Some(ts);
                times.push(ts);
                for (i, column) in table.columns.iter().enumerate() {
                    let value = match (column.kind, row.get_ref(i)?) {
                        (_, ValueRef::Null) => None,
                        (Kind::Integer, ValueRef::Integer(value)) => Some(value as u64),
                        (Kind::Real, ValueRef::Real(value)) if value.is_finite() => {
                            Some(value.to_bits())
                        }
                        _ => bail!(
                            "unsupported value in {}.{} at {ts}",
                            table.name,
                            column.name
                        ),
                    };
                    values[i].push(value);
                }
            }
            if times.is_empty() {
                break;
            }
            let mut block = Block {
                rows: times.len(),
                first: times[0],
                last: *times.last().unwrap(),
                columns: vec![],
            };
            for (ci, column) in values.into_iter().enumerate() {
                let (bytes,compressed) = stored_column(&column);
                let mut stats = Accumulator::default();
                for value in &column {
                    stats.add(value.map(|bits| match table.columns[ci].kind {
                        Kind::Integer => (bits as i64) as f64,
                        Kind::Real => f64::from_bits(bits),
                    }));
                }
                let offset = output.stream_position()?;
                output.write_all(&bytes)?;
                block.columns.push(ColumnBlock {
                    offset,
                    length: bytes.len(),
                    checksum: crc32fast::hash(&bytes),
                    stats: Some(stats.finish()),
                    compressed,
                });
            }
            table.rows += times.len() as u64;
            table.blocks.push(block);
        }
        manifest.tables.push(table);
    }
    let metadata = serde_json::to_vec(&manifest)?;
    ensure!(
        metadata.len() as u64 <= MAX_MANIFEST_BYTES,
        "index exceeds prototype limit"
    );
    let offset = output.stream_position()?;
    output.write_all(&metadata)?;
    output.seek(SeekFrom::Start(0))?;
    output.write_all(MAGIC)?;
    output.write_all(&offset.to_le_bytes())?;
    output.write_all(&(metadata.len() as u64).to_le_bytes())?;
    output.write_all(&crc32fast::hash(&metadata).to_le_bytes())?;
    output.as_file().sync_all()?;
    output
        .persist_noclobber(destination)
        .context("destination must not already exist")?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

impl Database {
    pub fn open(path: &Path, cache_limit: usize) -> Result<Self> {
        let mut file = File::open(path)?;
        let mut header = [0; HEADER_BYTES as usize];
        file.read_exact(&mut header)
            .context("truncated database header")?;
        ensure!(&header[..8] == MAGIC || &header[..8] == b"WWXDB001", "unsupported database format");
        let mut offset = u64::from_le_bytes(header[8..16].try_into()?);
        let mut length = u64::from_le_bytes(header[16..24].try_into()?);
        let mut checksum = u32::from_le_bytes(header[24..28].try_into()?);
        ensure!(
            length <= MAX_MANIFEST_BYTES
                && offset >= HEADER_BYTES
                && offset.checked_add(length).is_some_and(|end| end <= file.metadata().map_or(0, |m| m.len())),
            "invalid index bounds"
        );
        file.seek(SeekFrom::Start(offset))?;
        let mut metadata = vec![0; length as usize];
        file.read_exact(&mut metadata)?;
        ensure!(
            crc32fast::hash(&metadata) == checksum,
            "index checksum mismatch"
        );
        let mut manifest: Manifest = serde_json::from_slice(&metadata)?;
        if manifest.id.is_empty() { manifest.id = format!("legacy-{offset}-{checksum}"); }
        if let Some(head) = store::read_head(path)? {
            ensure!(head.id == manifest.id, "checkpoint belongs to another database");
            if head.lsn > manifest.lsn {
                offset = head.offset;
                length = head.length;
                checksum = head.checksum;
                ensure!(length <= MAX_MANIFEST_BYTES && offset >= HEADER_BYTES
                    && offset.checked_add(length).is_some_and(|end| end <= file.metadata().map_or(0, |m| m.len())), "invalid checkpoint bounds");
                file.seek(SeekFrom::Start(offset))?;
                metadata.resize(length as usize, 0);
                file.read_exact(&mut metadata)?;
                ensure!(crc32fast::hash(&metadata) == checksum, "checkpoint index checksum mismatch");
                let latest: Manifest = serde_json::from_slice(&metadata)?;
                ensure!(latest.id == manifest.id && latest.lsn == head.lsn, "invalid checkpoint identity");
                manifest = latest;
            }
        }
        for table in &manifest.tables {
            ensure!(
                table
                    .columns
                    .iter()
                    .any(|c| c.name == "dateTime" && matches!(c.kind, Kind::Integer)),
                "invalid timestamp column"
            );
            let mut previous = None;
            let mut row_count = 0;
            for block in &table.blocks {
                ensure!(
                    block.rows > 0
                        && block.rows <= BLOCK_ROWS
                        && block.first <= block.last
                        && previous.is_none_or(|p| block.first > p)
                    && block.columns.len() <= table.columns.len(),
                    "invalid block index"
                );
                for column in &block.columns {
                    ensure!(
                        (column.length == 0 || (!column.compressed && column.length == block_bytes(block.rows))
                            || (column.compressed && column.length < block_bytes(block.rows)))
                            && column.offset >= HEADER_BYTES
                            && column
                                .offset
                                .checked_add(column.length as u64)
                                .is_some_and(|end| end <= offset),
                        "invalid column bounds"
                    );
                    ensure!(
                        column.length != 0 || column.checksum == crc32fast::hash(&[]),
                        "invalid NULL column"
                    );
                }
                previous = Some(block.last);
                row_count += block.rows as u64;
            }
            ensure!(row_count == table.rows, "row count mismatch");
        }
        Ok(Self {
            file,
            manifest,
            cache: HashMap::new(),
            order: VecDeque::new(),
            cache_bytes: 0,
            cache_limit,
            metrics: ReadMetrics::default(),
        })
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
            cache_bytes: self.cache_bytes,
            cache_limit_bytes: self.cache_limit,
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
            return Ok(encode_full(&vec![default; block.rows]).into());
        }
        let column = &block.columns[ci];
        if column.length == 0 {
            return Ok(vec![0; block_bytes(block.rows)].into());
        }
        if let Some(bytes) = self.cache.get(&column.offset) {
            return Ok(bytes.clone());
        }
        let expected = block_bytes(block.rows);
        let compressed = column.compressed;
        let mut bytes = vec![0; column.length];
        self.file.seek(SeekFrom::Start(column.offset))?;
        self.file.read_exact(&mut bytes)?;
        self.metrics.column_reads += 1;
        self.metrics.bytes_read += bytes.len() as u64;
        ensure!(
            crc32fast::hash(&bytes) == column.checksum,
            "column checksum mismatch"
        );
        if compressed {
            bytes = lz4_flex::block::decompress(&bytes,expected).context("invalid compressed column")?;
            ensure!(bytes.len() == expected,"invalid decompressed column length");
        }
        let bytes: Arc<[u8]> = bytes.into();
        if bytes.len() <= self.cache_limit {
            while self.cache_bytes + bytes.len() > self.cache_limit {
                let oldest = self.order.pop_front().unwrap();
                self.cache_bytes -= self.cache.remove(&oldest).unwrap().len();
            }
            self.cache_bytes += bytes.len();
            self.order.push_back(column.offset);
            self.cache.insert(column.offset, bytes.clone());
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
            ensure!(
                (0..n).all(|i| bits(&times, n, i).is_some()),
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
        self.aggregate_excluding(table, column, span, &std::collections::BTreeSet::new())
    }

    fn aggregate_excluding(&mut self, table: &str, column: &str, span: Span, excluded: &std::collections::BTreeSet<i64>) -> Result<Stats> {
        ensure!(span.start <= span.stop, "reversed time span");
        let (ti, ci, ts) = self.table_column(table, column)?;
        let mut answer = Accumulator::default();
        for bi in 0..self.manifest.tables[ti].blocks.len() {
            let block = &self.manifest.tables[ti].blocks[bi];
            if span.before(block.last) || span.after(block.first) { continue; }
            if !span.before(block.first) && !span.after(block.last)
                && excluded.range(block.first..=block.last).next().is_none()
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
                if !span.before(time) && !span.after(time) && !excluded.contains(&time) {
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
                                .map(|(ci, bytes)| match bits(bytes, n, i) {
                                    None => Value::Null,
                                    Some(value) => {
                                        match self.manifest.tables[ti].columns[ci].kind {
                                            Kind::Integer => Value::Integer(value as i64),
                                            Kind::Real => Value::Real(f64::from_bits(value)),
                                        }
                                    }
                                });
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
        output.as_file().sync_all()?;
        output
            .persist_noclobber(destination)
            .context("destination must not already exist")?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    }
}
