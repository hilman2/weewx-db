// Copyright (c) 2026 Manuel Hilgert <hilman2@gmail.com>
//! The database file: one SQLite database in WAL mode that holds the column
//! blocks, their index and the rows not yet folded into blocks.
//!
//! SQLite provides atomic commits, crash recovery, locking between processes,
//! reuse of freed pages and consistent online backups. The Rust engine keeps
//! its own block format, summaries and caches; this module only moves bytes
//! between them and the file.
//!
//! Tables of the file:
//!
//! - `meta`: format facts and counters (`uuid`, `archive`, `seq`,
//!   `generation`, `next_chunk`, `next_table`).
//! - `tables`: numeric tables by ordinal. An ordinal is never reused, so rows
//!   of a dropped table can never reappear under a new table of that name.
//! - `metadata`: small tables without dateTime, such as `archive_day__metadata`.
//! - `blocks`: the block index, one row per block with its column descriptors.
//! - `chunks`: compressed column payloads by id. Ids are never reused.
//! - `garbage`: chunks replaced at a generation. They are deleted one
//!   compaction later, so a reader still holding the previous index can
//!   finish its query.
//! - `delta`: the current value of every row changed since the last
//!   compaction, or NULL for a deletion, with the transaction that wrote it.

use super::*;
use rusqlite::{OptionalExtension, config::DbConfig};
use std::path::PathBuf;
use std::time::Duration;

/// "WWDB" in PRAGMA application_id.
pub(crate) const APPLICATION_ID: i32 = 0x5757_4442;
pub(crate) const FORMAT_VERSION: i32 = 3;

const SCHEMA: &str = "
CREATE TABLE meta(key TEXT PRIMARY KEY, value) WITHOUT ROWID;
CREATE TABLE tables(ordinal INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, sql TEXT NOT NULL, columns BLOB NOT NULL);
CREATE TABLE metadata(name TEXT PRIMARY KEY, sql TEXT NOT NULL, rows BLOB NOT NULL) WITHOUT ROWID;
CREATE TABLE blocks(tab INTEGER NOT NULL, first INTEGER NOT NULL, last INTEGER NOT NULL, rows INTEGER NOT NULL,
    columns BLOB NOT NULL, PRIMARY KEY(tab, first)) WITHOUT ROWID;
CREATE TABLE chunks(id INTEGER PRIMARY KEY, payload BLOB NOT NULL);
CREATE TABLE garbage(chunk INTEGER PRIMARY KEY, generation INTEGER NOT NULL);
CREATE TABLE delta(time INTEGER NOT NULL, tab INTEGER NOT NULL, seq INTEGER NOT NULL, row BLOB,
    PRIMARY KEY(time, tab)) WITHOUT ROWID;
";

/// Every object a valid file contains. Opening refuses anything else, such as
/// triggers or views a crafted file could use to run statements.
const OBJECTS: &[(&str, &str)] = &[
    ("table", "meta"), ("table", "tables"), ("table", "metadata"), ("table", "blocks"),
    ("table", "chunks"), ("table", "garbage"), ("table", "delta"),
    ("index", "sqlite_autoindex_tables_1"),
];

/// Counters and identity from `meta`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Meta {
    pub uuid: String,
    pub archive: String,
    /// The last committed transaction.
    pub seq: u64,
    /// Changes whenever the block index or a schema changes.
    pub generation: u64,
}

/// A row of `delta`: table ordinal, dateTime, transaction and the row, or
/// None for a deletion.
pub(crate) type ChangedRow = (u64, i64, u64, Option<Vec<Cell>>);

pub(crate) struct Container {
    pub(crate) connection: Connection,
    pub(crate) path: PathBuf,
}

/// The file an open or create acts on. SQLite derives its -wal and -shm
/// names from it, so a symlink resolves to one database for every caller.
pub(crate) fn resolve(path: &Path) -> Result<PathBuf> {
    match std::fs::canonicalize(path) {
        Ok(path) => Ok(path),
        // A new file: resolve its directory instead.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
            let name = path.file_name().context("database path has no file name")?;
            Ok(std::fs::canonicalize(parent).with_context(|| format!("directory of {}", path.display()))?.join(name))
        }
        Err(e) => Err(e).with_context(|| format!("database {}", path.display())),
    }
}

impl Container {
    /// Create a new, empty database at `path`, which must not exist. The file
    /// is written under a temporary name and appears complete or not at all.
    pub(crate) fn create(path: &Path, archive: &str, fill: impl FnOnce(&mut Container) -> Result<()>) -> Result<()> {
        let path = resolve(path)?;
        ensure!(!path.exists(), "database already exists: {}", path.display());
        let temporary = sidecar(&path, &format!(".tmp-{}", new_id()?));
        let result = (|| -> Result<()> {
            {
                // SQLite creates the file with mode 0644 minus the umask, like
                // any other SQLite database of the station.
                let connection = Connection::open(&temporary)?;
                connection.pragma_update(None, "journal_mode", "OFF")?;
                connection.pragma_update(None, "synchronous", "OFF")?;
                connection.pragma_update(None, "application_id", APPLICATION_ID)?;
                connection.pragma_update(None, "user_version", FORMAT_VERSION)?;
                connection.execute_batch(SCHEMA)?;
                connection.execute_batch(&format!(
                    "INSERT INTO meta VALUES ('uuid', '{}'), ('archive', {}), ('seq', 0), ('generation', 0), ('next_chunk', 1), ('next_table', 1);",
                    new_id()?, sql_text(archive)))?;
                let mut container = Container { connection, path: temporary.clone() };
                container.connection.execute_batch("BEGIN")?;
                fill(&mut container)?;
                container.connection.execute_batch("COMMIT")?;
                // WAL lets readers continue while weewxd writes. The mode is
                // stored in the file.
                container.connection.pragma_update(None, "journal_mode", "WAL")?;
                container.connection.pragma_update(None, "synchronous", "FULL")?;
            }
            File::open(&temporary)?.sync_all()?;
            // Fails if `path` appeared meanwhile, instead of replacing it.
            std::fs::hard_link(&temporary, &path).map_err(|e| if e.kind() == std::io::ErrorKind::AlreadyExists {
                anyhow::anyhow!("destination must not already exist: {}", path.display())
            } else { anyhow::Error::new(e).context(format!("create {}", path.display())) })?;
            std::fs::remove_file(&temporary)?;
            sync_directory(&path)
        })();
        if result.is_err() {
            for suffix in ["", "-wal", "-shm", "-journal"] { let _ = std::fs::remove_file(sidecar(&temporary, suffix)); }
        }
        result
    }

    /// Open an existing database. `read_only` never writes, and falls back to
    /// SQLite's immutable mode where a directory without write access would
    /// not allow the shared-memory file of WAL mode.
    pub(crate) fn open(path: &Path, timeout: Duration, read_only: bool) -> Result<Self> {
        let path = resolve(path)?;
        ensure!(path.is_file(), "database does not exist: {}", path.display());
        let flags = OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI | if read_only {
            OpenFlags::SQLITE_OPEN_READ_ONLY } else { OpenFlags::SQLITE_OPEN_READ_WRITE };
        let connection = Connection::open_with_flags(&path, flags)
            .with_context(|| format!("open {}", path.display()))?;
        let container = Self { connection, path: path.clone() };
        let checked = container.configure(timeout).and_then(|_| container.validate()).and_then(|_| {
            // A copy made with VACUUM INTO is in rollback-journal mode.
            if !read_only { container.connection.pragma_update(None, "journal_mode", "WAL")?; }
            Ok(())
        });
        match checked {
            Err(error) if read_only && is_cannot_open(&error) => {
                let uri = format!("file:{}?immutable=1", percent_encode(&path));
                let connection = Connection::open_with_flags(&uri, flags)
                    .with_context(|| format!("open {} read-only", path.display()))?;
                let container = Self { connection, path };
                container.configure(timeout)?;
                container.validate()?;
                Ok(container)
            }
            Err(error) => Err(error),
            Ok(()) => Ok(container),
        }
    }

    fn configure(&self, timeout: Duration) -> Result<()> {
        self.connection.busy_timeout(timeout)?;
        // A crafted file must not run functions from views or triggers, nor
        // write the file's structures directly.
        self.connection.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)?;
        self.connection.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
        // Column payloads have their own cache; SQLite keeps only pages.
        self.connection.pragma_update(None, "cache_size", -2048)?;
        self.connection.pragma_update(None, "synchronous", "FULL")?;
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        let (application, version): (i32, i32) = self.connection.query_row(
            "SELECT application_id, user_version FROM pragma_application_id, pragma_user_version", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .with_context(|| format!("{} is not a weewx-db database", self.path.display()))?;
        if application != APPLICATION_ID {
            let mut header = [0u8; 8];
            let old = File::open(&self.path).and_then(|mut f| f.read_exact(&mut header)).is_ok() && header.starts_with(b"WWXDB");
            ensure!(!old, "{} uses the format of weewx-db 0.2; import the SQLite archive again", self.path.display());
            bail!("{} is not a weewx-db database", self.path.display());
        }
        ensure!(version == FORMAT_VERSION, "{} has format version {version}; this weewx-db reads version {FORMAT_VERSION}", self.path.display());
        let mut statement = self.connection.prepare("SELECT type, name FROM sqlite_schema ORDER BY name")?;
        let objects = statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (kind, name) in &objects {
            ensure!(OBJECTS.iter().any(|(k, n)| k == kind && n == name), "unexpected {kind} {name:?} in {}", self.path.display());
        }
        ensure!(objects.len() == OBJECTS.len(), "incomplete database structure in {}", self.path.display());
        Ok(())
    }

    pub(crate) fn set_synchronous(&self, mode: crate::store::SyncMode) -> Result<()> {
        let value = match mode { crate::store::SyncMode::Always => "FULL", crate::store::SyncMode::Background => "NORMAL" };
        self.connection.pragma_update(None, "synchronous", value)?;
        Ok(())
    }

    pub(crate) fn set_timeout(&self, timeout: Duration) -> Result<()> {
        self.connection.busy_timeout(timeout)?;
        Ok(())
    }

    /// Changes when another connection commits, cheaply and without I/O.
    pub(crate) fn data_version(&self) -> Result<i64> {
        Ok(self.connection.prepare_cached("PRAGMA data_version")?.query_row([], |r| r.get(0))?)
    }

    pub(crate) fn meta(&self) -> Result<Meta> {
        let value = |key: &str| -> Result<rusqlite::types::Value> {
            self.connection.prepare_cached("SELECT value FROM meta WHERE key = ?")?
                .query_row([key], |r| r.get(0)).optional()?.with_context(|| format!("meta value {key} missing"))
        };
        let text = |key: &str| -> Result<String> { match value(key)? {
            rusqlite::types::Value::Text(text) => Ok(text), _ => bail!("meta value {key} is not text") } };
        let number = |key: &str| -> Result<u64> { match value(key)? {
            rusqlite::types::Value::Integer(n) if n >= 0 => Ok(n as u64), _ => bail!("meta value {key} is not a count") } };
        Ok(Meta { uuid: text("uuid")?, archive: text("archive")?, seq: number("seq")?, generation: number("generation")? })
    }

    pub(crate) fn counter(&self, key: &str) -> Result<u64> {
        let value: i64 = self.connection.prepare_cached("SELECT value FROM meta WHERE key = ?")?.query_row([key], |r| r.get(0))?;
        u64::try_from(value).context("negative counter")
    }

    pub(crate) fn set_counter(&self, key: &str, value: u64) -> Result<()> {
        let value = i64::try_from(value).context("counter overflow")?;
        self.connection.prepare_cached("UPDATE meta SET value = ? WHERE key = ?")?.execute(rusqlite::params![value, key])?;
        Ok(())
    }

    /// Start a write transaction, waiting up to the busy timeout for another writer.
    pub(crate) fn begin_write(&self) -> Result<()> {
        self.connection.execute_batch("BEGIN IMMEDIATE").map_err(locked)
    }

    pub(crate) fn begin_read(&self) -> Result<()> {
        self.connection.execute_batch("BEGIN")?;
        Ok(())
    }

    pub(crate) fn commit(&self) -> Result<()> {
        self.connection.execute_batch("COMMIT").map_err(locked)
    }

    pub(crate) fn rollback(&self) {
        if !self.connection.is_autocommit() { let _ = self.connection.execute_batch("ROLLBACK"); }
    }

    /// The numeric tables with their block index, as of the current transaction.
    pub(crate) fn load_tables(&self) -> Result<Vec<Table>> {
        let mut tables = Vec::new();
        let mut statement = self.connection.prepare_cached("SELECT ordinal, name, sql, columns FROM tables ORDER BY ordinal")?;
        let rows = statement.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, Vec<u8>>(3)?)))?;
        for row in rows {
            let (ordinal, name, sql, columns) = row?;
            let columns: Vec<Column> = serde_json::from_slice(&columns).with_context(|| format!("columns of table {name}"))?;
            ensure!(columns.iter().any(|c| c.name == "dateTime" && c.kind == Kind::Integer), "table {name} has no INTEGER dateTime");
            validate_definition(&sql, &name)?;
            tables.push(Table { ordinal: ordinal as u64, name, sql, columns, blocks: vec![], rows: 0 });
        }
        let mut statement = self.connection.prepare_cached("SELECT tab, first, last, rows, columns FROM blocks ORDER BY tab, first")?;
        let rows = statement.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?, r.get::<_, Vec<u8>>(4)?)))?;
        for row in rows {
            let (ordinal, first, last, count, columns) = row?;
            let table = tables.iter_mut().find(|t| t.ordinal as i64 == ordinal).context("block of an unknown table")?;
            let count = usize::try_from(count).ok().filter(|n| (1..=BLOCK_ROWS).contains(n)).context("invalid block row count")?;
            ensure!(first <= last && table.blocks.last().is_none_or(|b: &Block| b.last < first), "invalid block order in {}", table.name);
            let columns = decode_columns(&columns)?;
            ensure!(columns.len() <= table.columns.len(), "block of {} has too many columns", table.name);
            for column in &columns {
                ensure!(column.length == 0 || column.chunk > 0, "invalid column descriptor in {}", table.name);
                let expected = block_bytes(count);
                ensure!(column.length == 0 || column.compressed || column.length == expected || column.length == expected + count.div_ceil(8),
                    "invalid column length in {}", table.name);
            }
            table.rows += count as u64;
            table.blocks.push(Block { rows: count, first, last, columns });
        }
        Ok(tables)
    }

    pub(crate) fn load_metadata(&self) -> Result<Vec<MetadataTable>> {
        let mut statement = self.connection.prepare_cached("SELECT name, sql, rows FROM metadata ORDER BY name")?;
        let rows = statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Vec<u8>>(2)?)))?;
        let mut metadata = Vec::new();
        for row in rows {
            let (name, sql, rows) = row?;
            validate_definition(&sql, &name)?;
            let rows: Vec<Vec<Cell>> = serde_json::from_slice(&rows).with_context(|| format!("rows of {name}"))?;
            metadata.push(MetadataTable { name, sql, rows });
        }
        Ok(metadata)
    }

    /// Rows of `delta` written after transaction `since`, in commit order.
    pub(crate) fn load_delta(&self, since: u64) -> Result<Vec<ChangedRow>> {
        let mut statement = self.connection.prepare_cached("SELECT tab, time, seq, row FROM delta WHERE seq > ? ORDER BY seq, tab, time")?;
        let rows = statement.query_map([i64::try_from(since)?], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, Option<Vec<u8>>>(3)?)))?;
        rows.map(|row| {
            let (tab, time, seq, row) = row?;
            Ok((tab as u64, time, seq as u64, row.map(|bytes| decode_row(&bytes)).transpose()?))
        }).collect()
    }

    pub(crate) fn read_chunk(&self, id: u64) -> Result<Vec<u8>> {
        self.connection.prepare_cached("SELECT payload FROM chunks WHERE id = ?")?
            .query_row([i64::try_from(id)?], |r| r.get(0)).optional()?
            .with_context(|| format!("column chunk {id} missing; the database changed, retry the query"))
    }

    pub(crate) fn write_chunk(&self, id: u64, payload: &[u8]) -> Result<()> {
        self.connection.prepare_cached("INSERT INTO chunks(id, payload) VALUES (?, ?)")?.execute(rusqlite::params![i64::try_from(id)?, payload])?;
        Ok(())
    }

    pub(crate) fn put_delta(&self, tab: u64, time: i64, seq: u64, row: Option<&[Cell]>) -> Result<()> {
        let row = row.map(encode_row);
        self.connection.prepare_cached("INSERT OR REPLACE INTO delta(time, tab, seq, row) VALUES (?, ?, ?, ?)")?
            .execute(rusqlite::params![time, i64::try_from(tab)?, i64::try_from(seq)?, row])?;
        Ok(())
    }

    pub(crate) fn write_table(&self, table: &Table) -> Result<()> {
        self.connection.prepare_cached("INSERT OR REPLACE INTO tables(ordinal, name, sql, columns) VALUES (?, ?, ?, ?)")?
            .execute(rusqlite::params![i64::try_from(table.ordinal)?, table.name, table.sql, serde_json::to_vec(&table.columns)?])?;
        Ok(())
    }

    /// Remove a table with its blocks and changed rows. Its chunks become
    /// garbage of `generation`.
    pub(crate) fn drop_table(&self, ordinal: u64, generation: u64) -> Result<()> {
        let ordinal = i64::try_from(ordinal)?;
        let mut chunks = Vec::new();
        {
            let mut statement = self.connection.prepare_cached("SELECT columns FROM blocks WHERE tab = ?")?;
            for columns in statement.query_map([ordinal], |r| r.get::<_, Vec<u8>>(0))? {
                chunks.extend(decode_columns(&columns?)?.into_iter().filter(|c| c.chunk > 0).map(|c| c.chunk));
            }
        }
        self.discard(&chunks, generation)?;
        self.connection.execute("DELETE FROM blocks WHERE tab = ?", [ordinal])?;
        self.connection.execute("DELETE FROM delta WHERE tab = ?", [ordinal])?;
        self.connection.execute("DELETE FROM tables WHERE ordinal = ?", [ordinal])?;
        Ok(())
    }

    pub(crate) fn write_metadata(&self, table: &MetadataTable) -> Result<()> {
        self.connection.prepare_cached("INSERT OR REPLACE INTO metadata(name, sql, rows) VALUES (?, ?, ?)")?
            .execute(rusqlite::params![table.name, table.sql, serde_json::to_vec(&table.rows)?])?;
        Ok(())
    }

    pub(crate) fn drop_metadata(&self, name: &str) -> Result<()> {
        self.connection.execute("DELETE FROM metadata WHERE name = ?", [name])?;
        Ok(())
    }

    /// Change the stored blocks of a table from `old` to `new`. Blocks present
    /// in both stay untouched, so a compaction writes only what it rewrote.
    pub(crate) fn write_blocks(&self, ordinal: u64, old: &[Block], new: &[Block]) -> Result<()> {
        let ordinal = i64::try_from(ordinal)?;
        // Chunk ids are never reused, so they identify a block's content.
        let key = |b: &Block| (b.first, b.last, b.rows, b.columns.iter().map(|c| c.chunk).collect::<Vec<_>>());
        let kept: std::collections::HashSet<_> = new.iter().map(key).collect();
        let existing: std::collections::HashSet<_> = old.iter().map(key).collect();
        let mut delete = self.connection.prepare_cached("DELETE FROM blocks WHERE tab = ? AND first = ?")?;
        for block in old.iter().filter(|b| !kept.contains(&key(b))) { delete.execute(rusqlite::params![ordinal, block.first])?; }
        let mut insert = self.connection.prepare_cached("INSERT OR REPLACE INTO blocks(tab, first, last, rows, columns) VALUES (?, ?, ?, ?, ?)")?;
        for block in new.iter().filter(|b| !existing.contains(&key(b))) {
            insert.execute(rusqlite::params![ordinal, block.first, block.last, block.rows as i64, encode_columns(&block.columns)])?;
        }
        Ok(())
    }

    pub(crate) fn discard(&self, chunks: &[u64], generation: u64) -> Result<()> {
        let mut insert = self.connection.prepare_cached("INSERT OR IGNORE INTO garbage(chunk, generation) VALUES (?, ?)")?;
        for &chunk in chunks { insert.execute(rusqlite::params![i64::try_from(chunk)?, i64::try_from(generation)?])?; }
        Ok(())
    }

    /// Delete chunks that became garbage before `generation`.
    pub(crate) fn purge(&self, generation: u64) -> Result<()> {
        let generation = i64::try_from(generation)?;
        self.connection.execute("DELETE FROM chunks WHERE id IN (SELECT chunk FROM garbage WHERE generation < ?)", [generation])?;
        self.connection.execute("DELETE FROM garbage WHERE generation < ?", [generation])?;
        Ok(())
    }

    pub(crate) fn clear_delta(&self) -> Result<()> {
        self.connection.execute("DELETE FROM delta", [])?;
        Ok(())
    }
}

fn locked(error: rusqlite::Error) -> anyhow::Error {
    let busy = matches!(error.sqlite_error_code(), Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked));
    let error = anyhow::Error::new(error);
    if busy { error.context("database is locked") } else { error }
}

fn is_cannot_open(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.downcast_ref::<rusqlite::Error>().and_then(rusqlite::Error::sqlite_error_code)
        .is_some_and(|code| matches!(code, rusqlite::ErrorCode::CannotOpen | rusqlite::ErrorCode::ReadOnly)))
}

fn percent_encode(path: &Path) -> String {
    path.to_string_lossy().bytes().map(|b| match b {
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
        _ => format!("%{b:02X}"),
    }).collect()
}

fn sql_text(text: &str) -> String { format!("'{}'", text.replace('\'', "''")) }

pub(crate) fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")))?.sync_all()?;
    Ok(())
}

pub(crate) fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    name.into()
}

/// A stored table definition is executed when a metadata table is created in
/// the SQL frontend and when exporting. It must be one CREATE TABLE statement
/// for the table of that name.
pub(crate) fn validate_definition(sql: &str, name: &str) -> Result<()> {
    use sqlparser::{ast::Statement, dialect::SQLiteDialect, parser::Parser};
    let statements = Parser::parse_sql(&SQLiteDialect {}, sql).with_context(|| format!("definition of {name}"))?;
    let [Statement::CreateTable(create)] = statements.as_slice() else { bail!("definition of {name} is not one CREATE TABLE statement") };
    let declared = create.name.to_string();
    ensure!(rusqlite::vtab::dequote(&declared).eq_ignore_ascii_case(name), "definition of {name} creates {declared}");
    Ok(())
}

const STATS: u8 = 1;
const COMPRESSED: u8 = 2;
const SUM: u8 = 4;
const MIN: u8 = 8;
const MAX: u8 = 16;

/// Column descriptors of one block: chunk id, payload length, CRC, flags
/// and the optional summary, little-endian.
fn encode_columns(columns: &[ColumnBlock]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(columns.len() * 49);
    for column in columns {
        bytes.extend_from_slice(&column.chunk.to_le_bytes());
        bytes.extend_from_slice(&(column.length as u32).to_le_bytes());
        bytes.extend_from_slice(&column.checksum.to_le_bytes());
        let stats = column.stats.as_ref();
        let flags = if column.compressed { COMPRESSED } else { 0 } | stats.map_or(0, |s| STATS
            | if s.sum.is_some() { SUM } else { 0 } | if s.min.is_some() { MIN } else { 0 } | if s.max.is_some() { MAX } else { 0 });
        bytes.push(flags);
        if let Some(stats) = stats {
            bytes.extend_from_slice(&stats.count.to_le_bytes());
            for value in [stats.sum, stats.min, stats.max] { bytes.extend_from_slice(&value.unwrap_or(0.0).to_bits().to_le_bytes()); }
        }
    }
    bytes
}

fn decode_columns(mut bytes: &[u8]) -> Result<Vec<ColumnBlock>> {
    fn take<'a>(bytes: &mut &'a [u8], n: usize) -> Result<&'a [u8]> {
        ensure!(bytes.len() >= n, "truncated column descriptor");
        let (head, tail) = bytes.split_at(n);
        *bytes = tail;
        Ok(head)
    }
    let mut columns = Vec::new();
    while !bytes.is_empty() {
        let chunk = u64::from_le_bytes(take(&mut bytes, 8)?.try_into()?);
        let length = u32::from_le_bytes(take(&mut bytes, 4)?.try_into()?) as usize;
        let checksum = u32::from_le_bytes(take(&mut bytes, 4)?.try_into()?);
        let flags = take(&mut bytes, 1)?[0];
        let stats = if flags & STATS != 0 {
            let count = u64::from_le_bytes(take(&mut bytes, 8)?.try_into()?);
            let mut value = |present: u8| -> Result<Option<f64>> {
                let bits = u64::from_le_bytes(take(&mut bytes, 8)?.try_into()?);
                Ok((flags & present != 0).then(|| f64::from_bits(bits)))
            };
            Some(Stats { count, sum: value(SUM)?, min: value(MIN)?, max: value(MAX)? })
        } else { None };
        columns.push(ColumnBlock { chunk, length, checksum, stats, compressed: flags & COMPRESSED != 0 });
    }
    Ok(columns)
}

/// A changed row: the cell count, then a tag and eight bytes per cell.
fn encode_row(row: &[Cell]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(2 + row.len() * 9);
    bytes.extend_from_slice(&(row.len() as u16).to_le_bytes());
    for cell in row {
        match cell {
            Cell::Integer(n) => { bytes.push(1); bytes.extend_from_slice(&n.to_le_bytes()); }
            Cell::Real(bits) => { bytes.push(2); bytes.extend_from_slice(&bits.to_le_bytes()); }
            // Numeric tables hold no text or blobs; validation rejects them.
            _ => bytes.push(0),
        }
    }
    bytes
}

fn decode_row(bytes: &[u8]) -> Result<Vec<Cell>> {
    ensure!(bytes.len() >= 2, "truncated row");
    let count = u16::from_le_bytes([bytes[0], bytes[1]]) as usize;
    let mut cells = Vec::with_capacity(count);
    let mut at = 2;
    for _ in 0..count {
        let tag = *bytes.get(at).context("truncated row")?;
        at += 1;
        cells.push(match tag {
            0 => Cell::Null,
            1 | 2 => {
                let value = u64::from_le_bytes(bytes.get(at..at + 8).context("truncated row")?.try_into()?);
                at += 8;
                if tag == 1 { Cell::Integer(value as i64) } else { Cell::Real(value) }
            }
            _ => bail!("invalid cell tag {tag}"),
        });
    }
    ensure!(at == bytes.len(), "trailing bytes in row");
    Ok(cells)
}

/// Whether a failure is the database's, not the caller's: an operating-system
/// error, a lock timeout, a full disk or a damaged file.
pub fn is_database_failure(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some()
            || cause.downcast_ref::<rusqlite::Error>().and_then(rusqlite::Error::sqlite_error_code).is_some_and(|code| !matches!(code,
                rusqlite::ErrorCode::ConstraintViolation | rusqlite::ErrorCode::TypeMismatch | rusqlite::ErrorCode::ApiMisuse
                | rusqlite::ErrorCode::ParameterOutOfRange | rusqlite::ErrorCode::Unknown | rusqlite::ErrorCode::InternalMalfunction))
            || cause.to_string() == "database is locked"
    })
}
