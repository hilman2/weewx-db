//! Which time ranges published transactions changed, per database file.
//!
//! WeeWX opens new connections for every report run. Results computed by an
//! earlier connection stay valid when no later transaction touched the time
//! range they read. The log is shared by all connections of this process;
//! transactions it never saw, such as those another process folded into a
//! compaction, count as changing everything.
//!
//! Consecutive transactions that touch nearby times of the same table merge
//! into one entry covering all of them. WeeWX rewrites today's row of every
//! daily-summary table and appends one archive record per interval, so each
//! table keeps about one entry per day instead of one per transaction. A
//! merged entry is conservative: it reports a change anywhere in its time
//! range for any transaction in its sequence range.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// The table and inclusive time range a transaction touched. Schema and
/// metadata changes cover the whole table.
pub(crate) type Range = (String, i64, i64);

/// Transactions older than this many sequence numbers are forgotten; results
/// that depend on them are recomputed.
const RETAINED: u64 = 100_000;
/// Entries kept per table. Dropping the oldest forgets its transactions.
const ENTRIES: usize = 512;
/// Appended times this close to an entry's range join it.
const MERGE_SECONDS: i64 = 2 * 86400;

struct Entry { first: u64, last: u64, lo: i64, hi: i64 }

struct Log {
    id: String,
    /// The contiguous run of recorded sequence numbers.
    known: Option<(u64, u64)>,
    /// Entries by lower-case table name, oldest first.
    tables: HashMap<String, Vec<Entry>>,
}

fn logs() -> &'static Mutex<HashMap<PathBuf, Log>> {
    static LOGS: OnceLock<Mutex<HashMap<PathBuf, Log>>> = OnceLock::new();
    LOGS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record what transaction `lsn` of database `id` at `path` changed.
pub(crate) fn record(path: &Path, id: &str, lsn: u64, ranges: Vec<Range>) {
    let Ok(mut logs) = logs().lock() else { return };
    let log = logs.entry(path.to_owned()).or_insert_with(|| Log { id: id.to_owned(), known: None, tables: HashMap::new() });
    if log.id != id { *log = Log { id: id.to_owned(), known: None, tables: HashMap::new() }; }
    match log.known {
        Some((first, last)) if lsn == last + 1 => log.known = Some((first, lsn)),
        // Already recorded, for example by another connection's reload.
        Some((first, last)) if (first..=last).contains(&lsn) => return,
        // A gap: nothing before this transaction is known any more.
        _ => { log.known = Some((lsn, lsn)); log.tables.clear(); }
    }
    let mut forgotten = lsn.saturating_sub(RETAINED);
    for (table, lo, hi) in ranges {
        let entries = log.tables.entry(table.to_ascii_lowercase()).or_default();
        match entries.last_mut() {
            Some(entry) if entry.last + 1 >= lsn && lo <= entry.hi.saturating_add(MERGE_SECONDS) && hi >= entry.lo.saturating_sub(MERGE_SECONDS) => {
                entry.last = lsn;
                entry.lo = entry.lo.min(lo);
                entry.hi = entry.hi.max(hi);
            }
            _ => entries.push(Entry { first: lsn, last: lsn, lo, hi }),
        }
        if entries.len() > ENTRIES { forgotten = forgotten.max(entries.remove(0).last); }
    }
    for entries in log.tables.values_mut() { entries.retain(|e| e.last > forgotten); }
    if let Some((first, last)) = log.known && first <= forgotten { log.known = Some((forgotten + 1, last)); }
}

/// Whether every transaction after `since` up to and including `until` is
/// known and none of them touched `table` within `lo..=hi`.
pub(crate) fn unchanged(path: &Path, id: &str, since: u64, until: u64, table: &str, lo: i64, hi: i64) -> bool {
    if until < since { return false; }
    if until == since { return true; }
    let Ok(logs) = logs().lock() else { return false };
    let Some(log) = logs.get(path).filter(|log| log.id == id) else { return false };
    let Some((first, last)) = log.known else { return false };
    if first > since + 1 || last < until { return false; }
    let entries = log.tables.get(&table.to_ascii_lowercase());
    !entries.is_some_and(|entries| entries.iter().any(|e| e.last > since && e.first <= until && e.lo <= hi && lo <= e.hi))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weewx_records_keep_about_one_entry_per_table_and_day() {
        let path = Path::new("/changes-test/weewx.wwdb");
        let day = 86400;
        for lsn in 1..=10 * 288u64 {
            let time = 1_700_006_400 + 300 * lsn as i64;
            let today = time - time % day;
            let mut ranges = vec![("archive".to_owned(), time, time)];
            ranges.extend((0..113).map(|i| (format!("archive_day_obs{i}"), today, today)));
            record(path, "id", lsn, ranges);
        }
        let logs = logs().lock().unwrap();
        let entries: usize = logs[path].tables.values().map(Vec::len).sum();
        assert!(entries <= 114 * 11, "{entries} entries");
        drop(logs);
        // An old span stays valid; the newest day changed.
        assert!(unchanged(path, "id", 1, 2880, "archive", 0, 1_700_000_000));
        assert!(!unchanged(path, "id", 2000, 2880, "archive", 1_700_006_400 + 300 * 2500, i64::MAX));
        assert!(!unchanged(path, "id", 2000, 2880, "ARCHIVE_DAY_OBS7", 1_700_006_400 + 9 * day, i64::MAX));
        // A transaction this process never saw makes everything unknown.
        record(path, "id", 2882, vec![]);
        assert!(!unchanged(path, "id", 2880, 2882, "archive", 0, 1));
    }
}
