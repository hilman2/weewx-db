//! Which time ranges each published transaction changed, per database file.
//!
//! WeeWX opens new connections for every report run. Results computed by an
//! earlier connection stay valid when no later transaction touched the time
//! range they read. The log is shared by all connections of this process;
//! transactions it never saw, such as frames another process folded into a
//! checkpoint, count as changing everything.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// The table and inclusive time range a transaction touched. Schema and
/// metadata changes cover the whole table.
pub(crate) type Range = (String, i64, i64);

/// Frames older than this many sequence numbers are forgotten; results that
/// depend on them are recomputed.
const RETAINED: usize = 100_000;

struct Log { id: String, frames: BTreeMap<u64, Vec<Range>> }

fn logs() -> &'static Mutex<HashMap<PathBuf, Log>> {
    static LOGS: OnceLock<Mutex<HashMap<PathBuf, Log>>> = OnceLock::new();
    LOGS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record what transaction `lsn` of database `id` at `path` changed.
pub(crate) fn record(path: &Path, id: &str, lsn: u64, ranges: Vec<Range>) {
    let Ok(mut logs) = logs().lock() else { return };
    let log = logs.entry(path.to_owned()).or_insert_with(|| Log { id: id.to_owned(), frames: BTreeMap::new() });
    if log.id != id { *log = Log { id: id.to_owned(), frames: BTreeMap::new() }; }
    log.frames.insert(lsn, ranges);
    while log.frames.len() > RETAINED { log.frames.pop_first(); }
}

/// Whether every transaction after `since` up to and including `until` is
/// known and none of them touched `table` within `lo..=hi`.
pub(crate) fn unchanged(path: &Path, id: &str, since: u64, until: u64, table: &str, lo: i64, hi: i64) -> bool {
    if until < since { return false; }
    if until == since { return true; }
    let Ok(logs) = logs().lock() else { return false };
    let Some(log) = logs.get(path).filter(|log| log.id == id) else { return false };
    let mut expected = since + 1;
    for (&lsn, ranges) in log.frames.range(since + 1..=until) {
        if lsn != expected { return false; }
        if ranges.iter().any(|(name, first, last)| name.eq_ignore_ascii_case(table) && *first <= hi && lo <= *last) { return false; }
        expected += 1;
    }
    expected == until + 1
}
