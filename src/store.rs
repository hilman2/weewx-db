//! Journaled mutations over immutable, indexed column blocks.

use super::*;
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const WAL_MAGIC: &[u8; 8] = b"WWXWAL03";
const RELAXED_FRAME: u32 = 1 << 31;
const MAX_FRAME: usize = 16 * 1024 * 1024;
const CHECKPOINT_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum SyncMode { Always, Background }

impl std::str::FromStr for SyncMode {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        match value { "always"=>Ok(Self::Always),"background"=>Ok(Self::Background),_=>bail!("sync_mode must be always or background") }
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Head {
    pub id: String,
    pub lsn: u64,
    pub offset: u64,
    pub length: u64,
    pub checksum: u32,
}

#[derive(Serialize, Deserialize)]
struct WalHeader { id: String, base_lsn: u64 }

#[derive(Clone, Serialize, Deserialize)]
enum Mutation {
    Put { table: String, time: i64, row: Option<Vec<Cell>> },
    Create { name: String, sql: String, columns: Vec<Column> },
    Metadata { name: String, sql: String, rows: Vec<Vec<Cell>> },
    Add { table: String, column: Column, sql: String },
    Drop { table: String },
}

/// Journal rows of one table. Cursors share the map; a write copies it only
/// while a cursor still reads the previous version.
pub(crate) type TableChanges = Arc<BTreeMap<i64, Option<Vec<Cell>>>>;
type Changes = BTreeMap<String, TableChanges>;

/// Identity, length and modification time of the main file and journal.
/// Every commit, checkpoint and vacuum changes at least one of them.
type Stamp = [Option<(u64, u64, std::time::SystemTime)>; 2];

pub fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    name.into()
}

pub(crate) fn read_head(path: &Path) -> Result<Option<Head>> {
    match std::fs::read(sidecar(path, ".head")) {
        Ok(bytes) => {
            ensure!(bytes.len() >= 4 && bytes.len() <= 4096, "invalid checkpoint pointer");
            let checksum = u32::from_le_bytes(bytes[..4].try_into()?);
            ensure!(crc32fast::hash(&bytes[4..]) == checksum, "checkpoint pointer checksum mismatch");
            Ok(Some(serde_json::from_slice(&bytes[4..])?))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn file_stamp(path: &Path) -> Result<Option<(u64, u64, std::time::SystemTime)>> {
    match std::fs::metadata(path) {
        #[cfg(unix)]
        Ok(meta) => Ok(Some((std::os::unix::fs::MetadataExt::ino(&meta), meta.len(), meta.modified()?))),
        #[cfg(not(unix))]
        Ok(meta) => Ok(Some((0, meta.len(), meta.modified()?))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None), Err(e) => Err(e.into()),
    }
}

fn parent(path: &Path) -> &Path {
    path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."))
}

fn sync_parent(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(parent(path))?.sync_all()?;
    Ok(())
}

fn write_header(output: &mut File, manifest: &Manifest) -> Result<()> {
    let bytes = serde_json::to_vec(manifest)?;
    ensure!(bytes.len() as u64 <= MAX_MANIFEST_BYTES, "index size limit exceeded");
    let offset = output.stream_position()?;
    output.write_all(&bytes)?;
    output.seek(SeekFrom::Start(0))?;
    output.write_all(MAGIC)?;
    output.write_all(&offset.to_le_bytes())?;
    output.write_all(&(bytes.len() as u64).to_le_bytes())?;
    output.write_all(&crc32fast::hash(&bytes).to_le_bytes())?;
    Ok(())
}

fn lock(path: &Path, exclusive: bool, timeout: Duration) -> Result<File> {
    let file = OpenOptions::new().read(true).write(true).create(true).truncate(false)
        .open(sidecar(path, ".lock"))?;
    let start = Instant::now();
    loop {
        let result = if exclusive { fs2::FileExt::try_lock_exclusive(&file) }
            else { fs2::FileExt::try_lock_shared(&file) };
        match result {
            Ok(()) => return Ok(file),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && start.elapsed() < timeout =>
                std::thread::sleep(Duration::from_millis(5)),
            Err(e) => return Err(e).context("database is locked"),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct StoreInfo {
    #[serde(flatten)]
    pub snapshot: Info,
    pub lsn: u64,
    pub journal_bytes: u64,
    pub changed_rows: usize,
    pub indexed_columns: usize,
    pub summarized_blocks: usize,
}

pub struct Store {
    pub(crate) snapshot: Database,
    pub(crate) changes: Changes,
    path: PathBuf,
    lsn: u64,
    valid_wal_bytes: u64,
    writer: Option<File>,
    pending: Vec<Mutation>,
    pending_bytes: usize,
    timeout: Duration,
    stamp: Stamp,
    pub workspace_bytes: usize,
    sync_mode: SyncMode,
    pending_sync: Option<(u64,std::thread::JoinHandle<std::io::Result<()>>)>,
    last_relaxed: bool,
    legacy_wal: bool,
    /// Results of `aggregate` for the current snapshot and journal. Reports
    /// repeat queries such as the first and last timestamp many times.
    aggregates: HashMap<(usize, String, i64, i64, bool), Stats>,
    /// Canonical path identifying this file in the process-wide change log.
    canonical: PathBuf,
}

impl Store {
    pub fn drop_database(path: &Path) -> Result<()> {
        let guard = lock(path,true,Duration::from_secs(5))?;
        Database::open(path,0)?;
        std::fs::remove_file(path)?;
        for suffix in [".head",".wal"] {
            match std::fs::remove_file(sidecar(path,suffix)) {
                Ok(()) => (), Err(e) if e.kind() == std::io::ErrorKind::NotFound => (), Err(e) => return Err(e.into()),
            }
        }
        sync_parent(path)?;
        drop(guard);
        Ok(())
    }
    /// Create an empty database; tables can then be created transactionally.
    pub fn create(path: &Path, archive: &str) -> Result<()> {
        let guard = lock(path,true,Duration::from_secs(5))?;
        ensure!(!path.exists(),"database already exists");
        ensure!(!sidecar(path,".wal").exists() && !sidecar(path,".head").exists(),"orphan journal or checkpoint exists; recover it before creating a database here");
        let mut output = tempfile::NamedTempFile::new_in(parent(path))?;
        output.write_all(&[0; HEADER_BYTES as usize])?;
        write_header(output.as_file_mut(), &Manifest {
            archive: archive.to_owned(), tables: vec![], metadata: vec![], id: new_id()?, lsn: 0,
        })?;
        output.as_file().sync_all()?;
        output.persist_noclobber(path)?;
        sync_parent(path)?;
        drop(guard);
        Ok(())
    }

    pub fn open(path: &Path, cache_bytes: usize) -> Result<Self> {
        Self::open_with_timeout(path, cache_bytes, Duration::from_secs(5))
    }

    /// Open, waiting at most `timeout` for a writer that holds the lock.
    pub fn open_with_timeout(path: &Path, cache_bytes: usize, timeout: Duration) -> Result<Self> {
        ensure!(path.is_file(), "database does not exist: {}", path.display());
        let guard = lock(path, false, timeout)?;
        let snapshot = Database::open(path, cache_bytes)?;
        let lsn = snapshot.manifest.lsn;
        let mut store = Self { snapshot, path: path.to_owned(), changes: BTreeMap::new(),
            lsn, valid_wal_bytes: 0, writer: None, pending: vec![], pending_bytes: 0,
            timeout, stamp: [None, None], workspace_bytes: 32*1024*1024,
            sync_mode:SyncMode::Always,pending_sync:None,last_relaxed:false,legacy_wal:false, aggregates: HashMap::new(),
            canonical: std::fs::canonicalize(path)? };
        store.replay()?;
        store.stamp = store.disk_stamp()?;
        drop(guard);
        Ok(store)
    }

    pub fn in_transaction(&self) -> bool { self.writer.is_some() }
    pub fn lsn(&self) -> u64 { self.lsn }
    pub fn set_sync_mode(&mut self, mode: SyncMode) -> Result<()> {
        if matches!(mode,SyncMode::Always) { self.flush()?; }
        self.sync_mode = mode;
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        if let Some((lsn,pending)) = self.pending_sync.take() {
            pending.join().map_err(|_|anyhow::anyhow!("journal synchronization thread panicked"))??;
            if lsn >= self.lsn { self.last_relaxed = false; }
        }
        if self.last_relaxed {
            OpenOptions::new().write(true).open(sidecar(&self.path,".wal"))?.sync_data()?;
            self.last_relaxed = false;
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
        Ok(())
    }

    pub fn refresh(&mut self) -> Result<()> {
        if self.in_transaction() { return Ok(()); }
        // Every published transaction appends to the journal, and every
        // checkpoint or vacuum replaces it. An unchanged journal therefore
        // means unchanged data; an interrupted checkpoint can change only the
        // main file, without changing what it contains. A writer still inside
        // its transaction is not visible yet either way, so this check needs
        // no lock. Writers compare both files in `begin`.
        if self.journal_stamp()? == self.stamp[1] { return Ok(()); }
        let guard = lock(&self.path, false, self.timeout)?;
        if self.disk_stamp()? != self.stamp { self.reload()?; }
        drop(guard);
        Ok(())
    }

    fn reload(&mut self) -> Result<()> {
        self.aggregates.clear();
        self.snapshot = Database::open(&self.path, self.snapshot.cache_limit)?;
        self.lsn = self.snapshot.manifest.lsn;
        self.changes.clear();
        self.pending.clear();
        self.pending_bytes = 0;
        self.replay()?;
        self.stamp = self.disk_stamp()?;
        Ok(())
    }

    fn disk_stamp(&self) -> Result<Stamp> {
        Ok([file_stamp(&self.path)?, self.journal_stamp()?])
    }

    fn journal_stamp(&self) -> Result<Option<(u64, u64, std::time::SystemTime)>> {
        file_stamp(&sidecar(&self.path, ".wal"))
    }

    fn replay(&mut self) -> Result<()> {
        self.last_relaxed = false;
        self.legacy_wal = false;
        let path = sidecar(&self.path, ".wal");
        if !path.exists() { self.valid_wal_bytes = 0; return Ok(()); }
        let mut input = File::open(path)?;
        let metadata = input.metadata()?;
        let length = metadata.len();
        if length == 0 { self.valid_wal_bytes = 0; return Ok(()); }
        #[cfg(unix)]
        let inode = std::os::unix::fs::MetadataExt::ino(&metadata);
        #[cfg(not(unix))]
        let inode = 0;
        let mut magic = [0; 8];
        input.read_exact(&mut magic).context("truncated journal header")?;
        self.legacy_wal = &magic == b"WWXWAL02";
        ensure!(&magic == WAL_MAGIC || self.legacy_wal, "invalid journal format");
        let mut size = [0; 4];
        input.read_exact(&mut size)?;
        let size = u32::from_le_bytes(size) as usize;
        ensure!(size <= 4096, "invalid journal header size");
        let mut header = vec![0; size];
        input.read_exact(&mut header)?;
        let mut checksum = [0; 4];
        input.read_exact(&mut checksum)?;
        ensure!(crc32fast::hash(&header) == u32::from_le_bytes(checksum), "journal header checksum mismatch");
        let header: WalHeader = serde_json::from_slice(&header)?;
        ensure!(header.id == self.snapshot.manifest.id && header.base_lsn <= self.lsn, "journal database identity or sequence mismatch");
        self.valid_wal_bytes = input.stream_position()?;
        let mut previous = header.base_lsn;
        // Continue from the state another connection of this process already
        // replayed. A journal only grows at its inode, so the replayed prefix
        // is unchanged; a reset journal is a new file.
        let base = (self.snapshot.manifest.id.clone(), self.snapshot.manifest.lsn, inode, header.base_lsn, self.legacy_wal);
        if let Some(state) = journals().lock().ok().and_then(|j| j.get(&self.canonical).filter(|s| s.base == base && s.valid <= length).cloned()) {
            self.changes = state.changes;
            self.snapshot.manifest = state.manifest;
            self.lsn = state.lsn;
            self.last_relaxed = state.last_relaxed;
            self.aggregates.clear();
            previous = state.previous;
            input.seek(SeekFrom::Start(state.valid))?;
            self.valid_wal_bytes = state.valid;
        }
        loop {
            let offset = input.stream_position()?;
            if length - offset < 16 { break; }
            let mut frame = [0; 16];
            input.read_exact(&mut frame)?;
            let tagged_size = u32::from_le_bytes(frame[..4].try_into()?);
            let relaxed = !self.legacy_wal && tagged_size & RELAXED_FRAME != 0;
            let size = (tagged_size & !RELAXED_FRAME) as usize;
            let checksum = u32::from_le_bytes(frame[4..8].try_into()?);
            let lsn = u64::from_le_bytes(frame[8..].try_into()?);
            ensure!(size > 0 && size <= MAX_FRAME, "invalid journal frame size");
            if length - input.stream_position()? < size as u64 { break; }
            let mut data = vec![0; size];
            input.read_exact(&mut data)?;
            let mut hash = crc32fast::Hasher::new();
            if !self.legacy_wal { hash.update(&frame[..4]); }
            hash.update(&lsn.to_le_bytes()); hash.update(&data);
            if hash.finalize() != checksum {
                // Only the newest background commit may be torn after its
                // acknowledgement. Every later writer syncs its predecessor
                // before appending another frame, even across processes.
                if relaxed && input.stream_position()? == length { break; }
                bail!("journal frame checksum mismatch at {offset}");
            }
            ensure!(previous.checked_add(1) == Some(lsn), "journal sequence gap");
            previous = lsn;
            if lsn > self.snapshot.manifest.lsn {
                let operations: Vec<Mutation> = serde_json::from_slice(&data)?;
                crate::changes::record(&self.canonical, &self.snapshot.manifest.id, lsn, touched(&operations));
                for operation in operations { self.apply(&operation)?; }
                self.lsn = lsn;
                self.last_relaxed = relaxed;
            }
            self.valid_wal_bytes = input.stream_position()?;
        }
        if previous < self.snapshot.manifest.lsn { self.valid_wal_bytes = 0; self.last_relaxed = false; return Ok(()); }
        if let Ok(mut journals) = journals().lock() {
            journals.insert(self.canonical.clone(), JournalState { base, valid: self.valid_wal_bytes, previous, lsn: self.lsn,
                last_relaxed: self.last_relaxed, changes: self.changes.clone(), manifest: self.snapshot.manifest.clone() });
        }
        Ok(())
    }

    fn reset_wal(&mut self) -> Result<File> {
        let mut file = tempfile::NamedTempFile::new_in(parent(&self.path))?;
        let bytes = serde_json::to_vec(&WalHeader { id: self.snapshot.manifest.id.clone(), base_lsn: self.lsn })?;
        file.write_all(WAL_MAGIC)?;
        file.write_all(&(bytes.len() as u32).to_le_bytes())?;
        file.write_all(&bytes)?;
        file.write_all(&crc32fast::hash(&bytes).to_le_bytes())?;
        file.as_file().sync_all()?;
        let length = file.stream_position()?;
        file.persist(sidecar(&self.path, ".wal"))?;
        sync_parent(&self.path)?;
        self.valid_wal_bytes = length;
        self.legacy_wal = false;
        self.last_relaxed = false;
        Ok(OpenOptions::new().read(true).write(true).open(sidecar(&self.path, ".wal"))?)
    }

    pub fn begin(&mut self) -> Result<()> { self.begin_with(false) }

    /// Begin a transaction without replacing the snapshot. Open read cursors
    /// hold block positions in it, so neither a reload nor the automatic
    /// checkpoint may run; the checkpoint waits for a later transaction.
    pub(crate) fn begin_keeping_snapshot(&mut self) -> Result<()> { self.begin_with(true) }

    fn begin_with(&mut self, keep_snapshot: bool) -> Result<()> {
        ensure!(!self.in_transaction(), "transaction already active");
        self.flush()?;
        let guard = lock(&self.path, true, self.timeout)?;
        if self.disk_stamp()? != self.stamp {
            ensure!(!keep_snapshot, "another connection changed the database while a read cursor is open; finish reading before writing");
            self.reload()?;
        }
        self.flush()?;
        if self.legacy_wal || (!keep_snapshot && self.valid_wal_bytes >= CHECKPOINT_BYTES) {
            ensure!(!keep_snapshot, "close read cursors before the first write to a legacy journal");
            self.checkpoint_locked(false)?;
        }
        if self.valid_wal_bytes == 0 { self.reset_wal()?; }
        let wal = OpenOptions::new().write(true).open(sidecar(&self.path, ".wal"))?;
        if wal.metadata()?.len() != self.valid_wal_bytes {
            wal.set_len(self.valid_wal_bytes)?;
            wal.sync_data()?;
        }
        self.writer = Some(guard);
        Ok(())
    }

    pub fn commit(&mut self) -> Result<()> {
        ensure!(self.in_transaction(), "no active transaction");
        if !self.pending.is_empty() {
            let mut frame = Vec::with_capacity(self.pending_bytes+16);
            frame.resize(16,0);
            serde_json::to_writer(&mut frame,&self.pending)?;
            ensure!(frame.len()-16 <= MAX_FRAME, "transaction exceeds 16 MiB journal limit");
            let lsn = self.lsn.checked_add(1).context("transaction sequence exhausted")?;
            let relaxed = matches!(self.sync_mode,SyncMode::Background);
            let size = ((frame.len()-16) as u32) | if relaxed { RELAXED_FRAME } else { 0 };
            frame[..4].copy_from_slice(&size.to_le_bytes());
            frame[8..16].copy_from_slice(&lsn.to_le_bytes());
            let mut hash = crc32fast::Hasher::new();
            hash.update(&frame[..4]); hash.update(&frame[8..]);
            frame[4..8].copy_from_slice(&hash.finalize().to_le_bytes());
            let mut wal = OpenOptions::new().append(true).open(sidecar(&self.path, ".wal"))?;
            wal.write_all(&frame)?;
            if relaxed {
                let sync_file = wal.try_clone()?;
                match std::thread::Builder::new().name("weewxdb-sync".into()).stack_size(64*1024).spawn(move||sync_file.sync_data()) {
                    Ok(worker) => { self.pending_sync = Some((lsn,worker)); self.last_relaxed = true; }
                    Err(_) => wal.sync_data()?,
                }
            } else {
                // fdatasync persists data and the length needed to retrieve it.
                // Creation and checkpoint publication also sync the directory.
                wal.sync_data()?;
            }
            self.valid_wal_bytes = wal.metadata()?.len();
            crate::changes::record(&self.canonical, &self.snapshot.manifest.id, lsn, touched(&self.pending));
            self.lsn = lsn;
        }
        self.pending.clear(); self.pending_bytes = 0;
        self.stamp = self.disk_stamp()?;
        self.writer.take();
        Ok(())
    }

    pub fn rollback(&mut self) -> Result<()> {
        ensure!(self.in_transaction(), "no active transaction");
        let result = self.reload();
        self.writer.take();
        result
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
        let size = serde_json::to_vec(&operation)?.len() + 1;
        ensure!(self.pending_bytes + size + 2 <= MAX_FRAME, "transaction exceeds 16 MiB journal limit; use smaller batches");
        self.apply(&operation)?;
        self.pending_bytes += size;
        self.pending.push(operation);
        Ok(())
    }

    fn apply(&mut self, operation: &Mutation) -> Result<()> {
        self.aggregates.clear();
        match operation {
            Mutation::Put { table, time, row } => {
                self.table_index(table)?;
                Arc::make_mut(self.changes.entry(table.clone()).or_default()).insert(*time, row.clone());
            }
            Mutation::Create { name, sql, columns } => {
                ensure!(!self.tables().contains(name), "table already exists: {name}");
                Arc::make_mut(&mut self.snapshot.manifest).tables.push(Table { name: name.clone(), sql: sql.clone(), columns: columns.clone(), blocks: vec![], rows: 0 });
            }
            Mutation::Metadata { name, sql, rows } => {
                Arc::make_mut(&mut self.snapshot.manifest).metadata.retain(|t| t.name != *name);
                Arc::make_mut(&mut self.snapshot.manifest).metadata.push(MetadataTable { name: name.clone(), sql: sql.clone(), rows: rows.clone() });
            }
            Mutation::Add { table, column, sql } => {
                let ti = self.table_index(table)?;
                ensure!(!self.snapshot.manifest.tables[ti].columns.iter().any(|c| c.name.eq_ignore_ascii_case(&column.name)), "duplicate column {}", column.name);
                Arc::make_mut(&mut self.snapshot.manifest).tables[ti].columns.push(column.clone());
                Arc::make_mut(&mut self.snapshot.manifest).tables[ti].sql = sql.clone();
                if let Some(changes) = self.changes.get_mut(table) {
                    for row in Arc::make_mut(changes).values_mut().flatten() { row.push(column.default.clone().unwrap_or(Cell::Null)); }
                }
            }
            Mutation::Drop { table } => {
                Arc::make_mut(&mut self.snapshot.manifest).tables.retain(|t| t.name != *table);
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
        self.stage(Mutation::Create { name: name.to_owned(), sql: sql.to_owned(), columns })
    }

    pub fn replace_metadata(&mut self, name: &str, sql: &str, rows: Vec<Vec<Cell>>) -> Result<()> {
        self.stage(Mutation::Metadata { name: name.to_owned(), sql: sql.to_owned(), rows })
    }

    pub fn add_column(&mut self, table: &str, column: Column, sql: &str) -> Result<()> {
        ensure!(!column.not_null || column.default.is_some(), "a new NOT NULL column needs a default");
        validate_cell(&column, column.default.as_ref().unwrap_or(&Cell::Null))?;
        self.stage(Mutation::Add { table: table.to_owned(), column, sql: sql.to_owned() })
    }

    pub fn drop_table(&mut self, table: &str) -> Result<()> {
        ensure!(self.tables().contains(&table.to_owned()), "no such table: {table}");
        self.stage(Mutation::Drop { table: table.to_owned() })
    }

    pub fn put(&mut self, table: &str, row: Vec<Cell>, replace: bool) -> Result<i64> {
        let columns = self.column_defs(table)?;
        ensure!(row.len() == columns.len(), "wrong column count");
        let mut normalized = Vec::with_capacity(row.len());
        for (column, cell) in columns.iter().zip(row) {
            let cell = numeric_cell(column.kind,cell);
            validate_cell(column, &cell)?;
            normalized.push(cell);
        }
        let index = columns.iter().position(|c| c.name == "dateTime").unwrap();
        let Cell::Integer(time) = normalized[index] else { bail!("dateTime must be an integer") };
        let table = self.snapshot.manifest.tables[self.table_index(table)?].name.clone();
        let table = table.as_str();
        ensure!(replace || self.get(table, time)?.is_none(), "UNIQUE constraint failed: {table}.dateTime");
        self.stage(Mutation::Put { table: table.to_owned(), time, row: Some(normalized) })?;
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
        self.stage(Mutation::Put { table: table.to_owned(), time, row: None })
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
        Ok(StoreInfo { snapshot: info, lsn: self.lsn, journal_bytes: self.valid_wal_bytes,
            changed_rows: self.changes.values().map(|c| c.len()).sum(),
            indexed_columns: self.snapshot.manifest.tables.iter().map(|t| t.columns.len()).sum(),
            summarized_blocks: self.snapshot.manifest.tables.iter().flat_map(|t| &t.blocks)
                .flat_map(|b| &b.columns).filter(|c| c.stats.is_some()).count() })
    }

    pub fn checkpoint(&mut self, rebuild_stats: bool) -> Result<()> {
        ensure!(!self.in_transaction(), "checkpoint cannot run inside a transaction");
        self.flush()?;
        let guard = lock(&self.path, true, self.timeout)?;
        self.reload()?;
        let result = self.checkpoint_locked(rebuild_stats);
        drop(guard);
        result
    }

    fn checkpoint_locked(&mut self, rebuild_stats: bool) -> Result<()> {
        let mut manifest = (*self.snapshot.manifest).clone();
        // Even an index-only optimization publishes a newer sequence number,
        // so an interrupted publication cannot make a reader select old data.
        manifest.lsn = self.lsn.checked_add(1).context("transaction sequence exhausted")?;
        let mut output = OpenOptions::new().read(true).append(true).open(&self.path)?;
        for (ti, table) in manifest.tables.iter_mut().enumerate() {
            let mut blocks = Vec::new();
            let mut lower = std::ops::Bound::Unbounded;
            let changes = self.changes.get(&table.name).cloned().unwrap_or_default();
            for bi in 0..table.blocks.len() {
                let block = &table.blocks[bi];
                let changed = changes.range((lower, std::ops::Bound::Included(block.last))).collect::<Vec<_>>();
                if changed.is_empty() && block.rows <= rows_per_block(&table.name) {
                    let mut block = block.clone();
                    if rebuild_stats {
                        for (ci,column) in block.columns.iter_mut().enumerate() {
                            let bytes = self.snapshot.load(ti,bi,ci)?;
                            let mut stats = Accumulator::default();
                            for ri in 0..block.rows { stats.add(number(&bytes,block.rows,ri,table.columns[ci].kind)); }
                            column.stats = Some(stats.finish());
                        }
                    }
                    blocks.push(block);
                } else {
                    let mut rows = BTreeMap::new();
                    let ts = table.columns.iter().position(|c| c.name == "dateTime").unwrap();
                    for row in self.read_block(ti, bi)? {
                        let Cell::Integer(time) = row[ts] else { bail!("invalid timestamp") };
                        rows.insert(time, row);
                    }
                    for (&time, row) in changed { match row { Some(row) => { rows.insert(time, row.clone()); }, None => { rows.remove(&time); } } }
                    append_rows(&mut output, &table.columns, rows.into_values(), &mut blocks,rows_per_block(&table.name))?;
                }
                lower = std::ops::Bound::Excluded(block.last);
            }
            let tail = changes.range((lower, std::ops::Bound::Unbounded)).filter_map(|(_, row)| row.clone());
            append_rows(&mut output, &table.columns, tail, &mut blocks,rows_per_block(&table.name))?;
            table.rows = blocks.iter().map(|b| b.rows as u64).sum();
            table.blocks = blocks;
        }
        let bytes = serde_json::to_vec(&manifest)?;
        ensure!(bytes.len() as u64 <= MAX_MANIFEST_BYTES, "index size limit exceeded");
        let offset = output.seek(SeekFrom::End(0))?;
        output.write_all(&bytes)?;
        output.sync_all()?;
        let head = Head { id: manifest.id.clone(), lsn: manifest.lsn, offset,
            length: bytes.len() as u64, checksum: crc32fast::hash(&bytes) };
        let bytes = serde_json::to_vec(&head)?;
        let mut pointer = tempfile::NamedTempFile::new_in(parent(&self.path))?;
        pointer.write_all(&crc32fast::hash(&bytes).to_le_bytes())?;
        pointer.write_all(&bytes)?;
        pointer.as_file().sync_all()?;
        pointer.persist(sidecar(&self.path, ".head"))?;
        sync_parent(&self.path)?;
        self.lsn = manifest.lsn;
        // A checkpoint republishes the same data under a new sequence number.
        crate::changes::record(&self.canonical, &manifest.id, manifest.lsn, vec![]);
        self.snapshot = Database::open(&self.path, self.snapshot.cache_limit)?;
        self.changes.clear();
        self.aggregates.clear();
        self.reset_wal()?;
        self.stamp = self.disk_stamp()?;
        Ok(())
    }

    pub fn verify(&mut self) -> Result<()> {
        for ti in 0..self.snapshot.manifest.tables.len() {
            for bi in 0..self.snapshot.manifest.tables[ti].blocks.len() {
                for ci in 0..self.snapshot.manifest.tables[ti].columns.len() { self.snapshot.load(ti,bi,ci)?; }
            }
        }
        Ok(())
    }

    pub fn vacuum(&mut self) -> Result<()> {
        ensure!(!self.in_transaction(), "vacuum cannot run inside a transaction");
        let guard = lock(&self.path,true,self.timeout)?;
        self.reload()?;
        self.checkpoint_locked(false)?;
        let mut manifest = (*self.snapshot.manifest).clone();
        manifest.lsn = self.lsn.checked_add(1).context("transaction sequence exhausted")?;
        let mut output = tempfile::NamedTempFile::new_in(parent(&self.path))?;
        output.write_all(&[0; HEADER_BYTES as usize])?;
        for (ti,table) in manifest.tables.iter_mut().enumerate() {
            for (bi,block) in table.blocks.iter_mut().enumerate() {
                for (ci,column) in block.columns.iter_mut().enumerate() {
                    let offset = output.stream_position()?;
                    if column.length > 0 {
                        self.snapshot.load(ti,bi,ci)?;
                        let original = &self.snapshot.manifest.tables[ti].blocks[bi].columns[ci];
                        self.snapshot.file.seek(SeekFrom::Start(original.offset))?;
                        let mut bytes = vec![0;original.length];
                        self.snapshot.file.read_exact(&mut bytes)?;
                        output.write_all(&bytes)?;
                    }
                    column.offset = offset;
                }
            }
        }
        write_header(output.as_file_mut(),&manifest)?;
        output.as_file().sync_all()?;
        output.persist(&self.path)?;
        sync_parent(&self.path)?;
        // The newer base LSN makes an old pointer harmless if a crash happens
        // between replacing the data file and removing that pointer.
        std::fs::remove_file(sidecar(&self.path,".head"))?;
        self.snapshot = Database::open(&self.path,self.snapshot.cache_limit)?;
        self.aggregates.clear();
        self.lsn = manifest.lsn;
        crate::changes::record(&self.canonical, &manifest.id, manifest.lsn, vec![]);
        self.reset_wal()?;
        self.stamp = self.disk_stamp()?;
        drop(guard);
        Ok(())
    }

    pub fn export_sqlite(&mut self, destination: &Path) -> Result<()> {
        let output = tempfile::NamedTempFile::new_in(parent(destination))?;
        {
            let mut connection = Connection::open(output.path())?;
            let transaction = connection.transaction()?;
            let manifest = self.snapshot.manifest.clone();
            for table in &manifest.tables {
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
        output.as_file().sync_all()?;
        output.persist_noclobber(destination)?;
        sync_parent(destination)
    }
}

impl Drop for Store {
    fn drop(&mut self) { let _ = self.flush(); }
}

/// A replayed journal: the published snapshot and journal it starts from,
/// how far it was read, and the resulting rows and schema.
#[derive(Clone)]
struct JournalState {
    /// Database id and snapshot sequence, journal inode, journal base
    /// sequence and whether the journal has the legacy format.
    base: (String, u64, u64, u64, bool),
    valid: u64,
    previous: u64,
    lsn: u64,
    last_relaxed: bool,
    changes: Changes,
    manifest: Arc<Manifest>,
}

/// Forget the replayed journal this process keeps for `path`, as a new
/// process would. Frames are verified once per process; tests use this to
/// check how a fresh process treats a damaged journal.
#[doc(hidden)]
pub fn forget_replayed_journal(path: &Path) {
    if let (Ok(path), Ok(mut journals)) = (std::fs::canonicalize(path), journals().lock()) { journals.remove(&path); }
}

fn journals() -> &'static std::sync::Mutex<HashMap<PathBuf, JournalState>> {
    static JOURNALS: std::sync::OnceLock<std::sync::Mutex<HashMap<PathBuf, JournalState>>> = std::sync::OnceLock::new();
    JOURNALS.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// The tables and time ranges a transaction's mutations change.
fn touched(operations: &[Mutation]) -> Vec<crate::changes::Range> {
    let mut ranges: Vec<crate::changes::Range> = Vec::new();
    for operation in operations {
        let (table, first, last) = match operation {
            Mutation::Put { table, time, .. } => (table, *time, *time),
            Mutation::Create { name, .. } | Mutation::Metadata { name, .. } => (name, i64::MIN, i64::MAX),
            Mutation::Add { table, .. } | Mutation::Drop { table } => (table, i64::MIN, i64::MAX),
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

fn validate_cell(column: &Column, value: &Cell) -> Result<()> {
    ensure!(!column.not_null || *value != Cell::Null, "NOT NULL constraint failed: {}", column.name);
    ensure!(matches!((column.kind, value), (_, Cell::Null) | (Kind::Integer, Cell::Integer(_)))
        || matches!((column.kind, value), (Kind::Real, Cell::Real(bits)) if f64::from_bits(*bits).is_finite()), "invalid value for {}", column.name);
    Ok(())
}

fn decode(bytes: &[u8], rows: usize, ri: usize, kind: Kind) -> Cell {
    match bits(bytes, rows, ri) { None => Cell::Null, Some(n) => match kind {
        Kind::Integer => Cell::Integer(n as i64), Kind::Real => Cell::Real(n),
    } }
}

fn append_rows(output: &mut File, columns: &[Column], rows: impl Iterator<Item=Vec<Cell>>, blocks: &mut Vec<Block>,limit: usize) -> Result<()> {
    let mut rows = rows.peekable();
    let ts = columns.iter().position(|c| c.name == "dateTime").context("dateTime missing")?;
    while rows.peek().is_some() {
        let rows = rows.by_ref().take(limit).collect::<Vec<_>>();
        let times = rows.iter().map(|r| match r[ts] { Cell::Integer(n) => Ok(n), _ => bail!("invalid timestamp") }).collect::<Result<Vec<_>>>()?;
        let mut block = Block { rows: rows.len(), first: times[0], last: *times.last().unwrap(), columns: vec![] };
        for (ci, column) in columns.iter().enumerate() {
            let values = rows.iter().map(|row| match &row[ci] {
                Cell::Null => None, Cell::Integer(n) => Some(*n as u64), Cell::Real(n) => Some(*n), _ => unreachable!(),
            }).collect::<Vec<_>>();
            let mut stats = Accumulator::default();
            for value in &values { stats.add(value.map(|v| match column.kind { Kind::Integer => (v as i64) as f64, Kind::Real => f64::from_bits(v) })); }
            let (bytes,compressed) = stored_column(&values);
            let offset = output.seek(SeekFrom::End(0))?;
            output.write_all(&bytes)?;
            block.columns.push(ColumnBlock { offset, length: bytes.len(), checksum: crc32fast::hash(&bytes), stats: Some(stats.finish()), compressed });
        }
        blocks.push(block);
    }
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
