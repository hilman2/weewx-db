use std::hint::black_box;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use rusqlite::Connection;
use serde::Serialize;
use weewx_db::{Span, Stats, open_sqlite, quote};
use weewx_db::store::Store;

#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
enum Answer {
    Aggregate(Stats),
    Buckets(Vec<Stats>),
    Daily(Option<f64>),
}

struct Workload {
    name: &'static str,
    table: String,
    column: String,
    start: i64,
    stop: i64,
    seconds: Option<i64>,
    daily: bool,
}

impl Workload {
    fn custom(&self, db: &mut Store) -> Result<Answer> {
        let span = Span {
            start: self.start,
            stop: self.stop,
            daily: false,
        };
        Ok(if self.daily {
            Answer::Daily(db.daily_mean(&self.table, self.start, self.stop)?)
        } else if let Some(seconds) = self.seconds {
            Answer::Buckets(db.buckets(&self.table, &self.column, span, seconds)?)
        } else {
            Answer::Aggregate(db.aggregate(&self.table, &self.column, span)?)
        })
    }

    fn sqlite(&self, db: &Connection) -> Result<Answer> {
        let table = quote(&self.table);
        if self.daily {
            let mut statement = db.prepare_cached(&format!(
                "SELECT SUM(wsum)/SUM(sumtime) FROM {table} WHERE dateTime>=? AND dateTime<?"
            ))?;
            return Ok(Answer::Daily(
                statement.query_row([self.start, self.stop], |row| row.get(0))?,
            ));
        }
        let column = quote(&self.column);
        let stats = format!("COUNT({column}), SUM({column}), MIN({column}), MAX({column})");
        if let Some(seconds) = self.seconds {
            // One grouped SELECT is a stronger baseline than one SQL query per
            // hour. Both engines must return the same explicit empty bins.
            let mut statement = db.prepare_cached(&format!(
                "SELECT (dateTime-?1-1)/?3 AS bucket, {stats} FROM {table}
                 WHERE dateTime>?1 AND dateTime<=?2 GROUP BY bucket ORDER BY bucket"
            ))?;
            let mut answers =
                vec![Stats::default(); ((self.stop - self.start + seconds - 1) / seconds) as usize];
            let mut rows = statement.query([self.start, self.stop, seconds])?;
            while let Some(row) = rows.next()? {
                let index = usize::try_from(row.get::<_, i64>(0)?)?;
                answers[index] = read_stats(row, 1)?;
            }
            Ok(Answer::Buckets(answers))
        } else {
            let mut statement = db.prepare_cached(&format!(
                "SELECT {stats} FROM {table} WHERE dateTime>? AND dateTime<=?"
            ))?;
            Ok(Answer::Aggregate(
                statement.query_row([self.start, self.stop], |r| read_stats(r, 0))?,
            ))
        }
    }
}

fn read_stats(row: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<Stats> {
    Ok(Stats {
        count: row.get::<_, i64>(offset)? as u64,
        sum: row.get(offset + 1)?,
        min: row.get(offset + 2)?,
        max: row.get(offset + 3)?,
    })
}

fn same_number(a: Option<f64>, b: Option<f64>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            a.is_finite() && b.is_finite() && (a - b).abs() <= 1e-9 + 1e-10 * a.abs().max(b.abs())
        }
        _ => false,
    }
}

fn same_stats(a: &Stats, b: &Stats) -> bool {
    a.count == b.count && same_number(a.sum, b.sum) && a.min == b.min && a.max == b.max
}

fn check(a: &Answer, b: &Answer) -> Result<()> {
    let equal = match (a, b) {
        (Answer::Aggregate(a), Answer::Aggregate(b)) => same_stats(a, b),
        (Answer::Buckets(a), Answer::Buckets(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same_stats(a, b))
        }
        (Answer::Daily(a), Answer::Daily(b)) => same_number(*a, *b),
        _ => false,
    };
    ensure!(equal, "result mismatch between SQLite and weewx-db");
    Ok(())
}

#[derive(Serialize)]
pub struct Case {
    workload: String,
    first_sqlite_ms: f64,
    first_weewxdb_ms: f64,
    sqlite_median_ms: f64,
    weewxdb_median_ms: f64,
    speedup: f64,
    verified_answers: usize,
}

#[derive(Serialize)]
pub struct Report {
    platform: String,
    sqlite_version: String,
    source: String,
    database: String,
    archive_rows: u64,
    source_bytes: u64,
    database_bytes: u64,
    cache_limit_bytes_per_engine: usize,
    custom_cache_resident_bytes: usize,
    rounds: usize,
    cache_note: &'static str,
    cases: Vec<Case>,
}

fn timed<T>(run: impl FnOnce() -> Result<T>) -> Result<(T, f64)> {
    let start = Instant::now();
    let result = black_box(run()?);
    Ok((result, start.elapsed().as_secs_f64() * 1000.0))
}

fn median(samples: &mut [f64]) -> f64 {
    samples.sort_by(f64::total_cmp);
    let middle = samples.len() / 2;
    if samples.len().is_multiple_of(2) {
        (samples[middle - 1] + samples[middle]) / 2.0
    } else {
        samples[middle]
    }
}

pub fn run(
    source: &Path,
    database: &Path,
    cache_bytes: usize,
    rounds: usize,
    column: &str,
) -> Result<Report> {
    ensure!((1..=1000).contains(&rounds), "rounds must be 1..1000");
    let mut custom = Store::open(database, cache_bytes)?;
    let info = custom.info()?.snapshot;
    drop(custom);
    let first = info.first.context("empty archive")?;
    let last = info.last.context("empty archive")?;
    let source_check = open_sqlite(source, cache_bytes)?;
    let (rows, source_first, source_last): (i64, i64, i64) = source_check.query_row(
        &format!(
            "SELECT COUNT(*), MIN(dateTime), MAX(dateTime) FROM {}",
            quote(&info.archive)
        ),
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    ensure!(
        rows as u64 == info.rows && source_first == first && source_last == last,
        "source and imported archive bounds differ"
    );
    drop(source_check);
    let mut workloads = Vec::new();
    for (name, days, seconds) in [
        ("raw_day", 1, None),
        ("raw_month", 30, None),
        ("hourly_year", 365, Some(3600)),
    ] {
        workloads.push(Workload {
            name,
            table: info.archive.clone(),
            column: column.to_owned(),
            start: last.saturating_sub(days * 86400),
            stop: last,
            seconds,
            daily: false,
        });
    }
    workloads.push(Workload {
        name: "raw_entire_archive",
        table: info.archive.clone(),
        column: column.to_owned(),
        start: first.saturating_sub(1),
        stop: last,
        seconds: None,
        daily: false,
    });
    let daily_table = format!("{}_day_{column}", info.archive);
    if info.tables.contains(&daily_table) {
        // Use the existing daily table, including its time weights. Comparing
        // this report query against a full raw scan would exaggerate the gain.
        workloads.push(Workload {
            name: "stored_daily_year_mean",
            table: daily_table,
            column: column.to_owned(),
            start: last.saturating_sub(365 * 86400),
            stop: last,
            seconds: None,
            daily: true,
        });
    }
    let mut report = Report {
        platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        sqlite_version: rusqlite::version().to_owned(),
        source: source.display().to_string(),
        database: database.display().to_string(),
        archive_rows: info.rows,
        source_bytes: source.metadata()?.len(),
        database_bytes: database.metadata()?.len(),
        cache_limit_bytes_per_engine: cache_bytes,
        custom_cache_resident_bytes: 0,
        rounds,
        cache_note: "First queries use new connections and empty engine caches. OS caches are not cleared. Medians follow one warmup; engine order alternates. Cache budgets exclude indices, results, SQLite overhead and OS cache. SQLite statements are cached and mmap is disabled. Read-only microbenchmark, not report or durable write throughput.",
        cases: vec![],
    };
    for (case_index, workload) in workloads.iter().enumerate() {
        let mut custom = Store::open(database, cache_bytes)?;
        let sqlite = open_sqlite(source, cache_bytes)?;
        let (first_sqlite, first_custom) = if case_index.is_multiple_of(2) {
            let sqlite_answer = timed(|| workload.sqlite(&sqlite))?;
            let custom_answer = timed(|| workload.custom(&mut custom))?;
            (sqlite_answer, custom_answer)
        } else {
            let custom_answer = timed(|| workload.custom(&mut custom))?;
            let sqlite_answer = timed(|| workload.sqlite(&sqlite))?;
            (sqlite_answer, custom_answer)
        };
        check(&first_sqlite.0, &first_custom.0).with_context(|| workload.name)?;
        check(&workload.sqlite(&sqlite)?, &workload.custom(&mut custom)?)?;
        let mut sql_samples = Vec::new();
        let mut custom_samples = Vec::new();
        for round in 0..rounds {
            let (sql, own) = if round.is_multiple_of(2) {
                let sql = timed(|| workload.sqlite(&sqlite))?;
                let own = timed(|| workload.custom(&mut custom))?;
                (sql, own)
            } else {
                let own = timed(|| workload.custom(&mut custom))?;
                let sql = timed(|| workload.sqlite(&sqlite))?;
                (sql, own)
            };
            check(&sql.0, &own.0).with_context(|| workload.name)?;
            sql_samples.push(sql.1);
            custom_samples.push(own.1);
        }
        let sql_median = median(&mut sql_samples);
        let custom_median = median(&mut custom_samples);
        report.custom_cache_resident_bytes = report
            .custom_cache_resident_bytes
            .max(custom.info()?.snapshot.cache_bytes);
        report.cases.push(Case {
            workload: workload.name.to_owned(),
            first_sqlite_ms: first_sqlite.1,
            first_weewxdb_ms: first_custom.1,
            sqlite_median_ms: sql_median,
            weewxdb_median_ms: custom_median,
            speedup: sql_median / custom_median,
            verified_answers: rounds + 2,
        });
    }
    Ok(report)
}
