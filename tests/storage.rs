use std::fs;
use std::path::Path;

use rusqlite::types::Value;
use rusqlite::{Connection, params};
use weewx_db::{Database, Span, Stats, import_sqlite, quote};

fn source(path: &Path) -> Connection {
    let mut connection = Connection::open(path).unwrap();
    connection.execute_batch(
        "CREATE TABLE archive(usUnits INTEGER, outTemp REAL, dateTime INTEGER PRIMARY KEY,
            interval INTEGER, rain REAL, absent REAL, counter INTEGER);
         CREATE TABLE archive_day_outTemp(dateTime INTEGER PRIMARY KEY, min REAL, max REAL,
            wsum REAL, sumtime INTEGER);
         INSERT INTO archive_day_outTemp VALUES (0, -999.5, 999.5, 20, 1), (86400, -777, 888, 90, 3);
         CREATE TABLE archive_day__metadata(name CHAR(20) PRIMARY KEY, value TEXT, extra BLOB);
         INSERT INTO archive_day__metadata VALUES ('Version','4.0',X'0001FF'), ('lastUpdate','86400',NULL);"
    ).unwrap();
    let transaction = connection.transaction().unwrap();
    {
        let mut insert = transaction
            .prepare("INSERT INTO archive VALUES (17,?,?,?, ?,NULL,?)")
            .unwrap();
        for i in 0_i64..7000 {
            if i % 19 == 0 {
                continue;
            }
            let value = if i % 13 == 0 {
                None
            } else {
                Some((i % 31 - 15) as f64 * 0.25)
            };
            insert
                .execute(params![
                    value,
                    i * 300 - 900_000,
                    if i % 7 == 0 { 2 } else { 5 },
                    if i % 113 == 0 { 0.4 } else { 0.0 },
                    i64::MAX - i
                ])
                .unwrap();
        }
    }
    transaction.commit().unwrap();
    connection
}

fn reference(connection: &Connection, column: &str, span: Span) -> Stats {
    let column = quote(column);
    let (left, right) = if span.daily { (">=", "<") } else { (">", "<=") };
    connection
        .query_row(
            &format!(
                "SELECT COUNT({column}), SUM({column}), MIN({column}), MAX({column})
        FROM archive WHERE dateTime{left}? AND dateTime{right}?"
            ),
            [span.start, span.stop],
            |row| {
                Ok(Stats {
                    count: row.get::<_, i64>(0)? as u64,
                    sum: row.get(1)?,
                    min: row.get(2)?,
                    max: row.get(3)?,
                })
            },
        )
        .unwrap()
}

fn close(a: &Stats, b: &Stats) {
    assert_eq!(a.count, b.count);
    assert_eq!(a.min, b.min);
    assert_eq!(a.max, b.max);
    match (a.sum, b.sum) {
        (Some(a), Some(b)) => assert!((a - b).abs() < 1e-9 + 1e-12 * a.abs()),
        (None, None) => (),
        _ => panic!("NULL sum mismatch: {a:?} != {b:?}"),
    }
}

#[test]
fn sqlite_differential_spans_nulls_zeroes_and_bounded_cache() {
    let dir = tempfile::tempdir().unwrap();
    let sqlite = dir.path().join("source.sdb");
    let own = dir.path().join("archive.wwdb");
    let connection = source(&sqlite);
    import_sqlite(&sqlite, &own, "archive").unwrap();
    for limit in [0, 8 * 1024, 70 * 1024] {
        let mut db = Database::open(&own, limit).unwrap();
        let mut seed = 12345_u64;
        for i in 0..80 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let start = (seed % 2_500_000) as i64 - 1_000_000;
            let stop = start + (seed.rotate_left(17) % 1_000_000) as i64;
            let span = Span {
                start,
                stop,
                daily: i % 2 == 0,
            };
            for column in ["outTemp", "rain", "absent", "interval"] {
                close(
                    &db.aggregate("archive", column, span).unwrap(),
                    &reference(&connection, column, span),
                );
                assert!(db.info().cache_bytes <= limit);
            }
        }
        let zero = Span {
            start: 0,
            stop: 0,
            daily: false,
        };
        assert_eq!(
            db.aggregate("archive", "rain", zero).unwrap(),
            Stats::default()
        );
        let excluded = db
            .aggregate(
                "archive",
                "dateTime",
                Span {
                    start: 0,
                    stop: 300,
                    daily: false,
                },
            )
            .unwrap();
        assert_eq!(
            excluded,
            Stats {
                count: 1,
                sum: Some(300.0),
                min: Some(300.0),
                max: Some(300.0)
            }
        );
    }
}

#[test]
fn buckets_match_sqlite_at_boundaries_gaps_and_partial_final_bin() {
    let dir = tempfile::tempdir().unwrap();
    let sqlite = dir.path().join("source.sdb");
    let own = dir.path().join("archive.wwdb");
    let connection = source(&sqlite);
    import_sqlite(&sqlite, &own, "archive").unwrap();
    let mut db = Database::open(&own, 8192).unwrap();
    for (start, stop, seconds) in [(-1_100_000, 1_500_121, 3600), (0, 601, 300), (0, 0, 300)] {
        let span = Span {
            start,
            stop,
            daily: false,
        };
        let buckets = db.buckets("archive", "outTemp", span, seconds).unwrap();
        for (i, bucket) in buckets.iter().enumerate() {
            let begin = start + i as i64 * seconds;
            close(
                bucket,
                &reference(
                    &connection,
                    "outTemp",
                    Span {
                        start: begin,
                        stop: (begin + seconds).min(stop),
                        daily: false,
                    },
                ),
            );
        }
        assert_eq!(buckets.len() as i64, (stop - start + seconds - 1) / seconds);
    }
    assert!(
        db.buckets(
            "archive",
            "outTemp",
            Span {
                start: 0,
                stop: 300,
                daily: false
            },
            0
        )
        .is_err()
    );
    assert!(
        db.buckets(
            "archive",
            "outTemp",
            Span {
                start: i64::MIN,
                stop: i64::MAX,
                daily: false
            },
            1
        )
        .is_err()
    );
    assert!(
        db.aggregate(
            "archive",
            "outTemp",
            Span {
                start: 1,
                stop: 0,
                daily: false
            }
        )
        .is_err()
    );
    assert!(
        db.aggregate(
            "archive",
            "unknown",
            Span {
                start: 0,
                stop: 1,
                daily: false
            }
        )
        .is_err()
    );
}

fn table_rows(connection: &Connection, table: &str) -> Vec<Vec<Value>> {
    let mut statement = connection
        .prepare(&format!("SELECT * FROM {} ORDER BY 1", quote(table)))
        .unwrap();
    let count = statement.column_count();
    statement
        .query_map([], |row| (0..count).map(|i| row.get(i)).collect())
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

#[test]
fn roundtrip_preserves_integer_precision_loop_extrema_and_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let sqlite = dir.path().join("source.sdb");
    let own = dir.path().join("archive.wwdb");
    let restored = dir.path().join("restored.sdb");
    let connection = source(&sqlite);
    import_sqlite(&sqlite, &own, "archive").unwrap();
    let mut db = Database::open(&own, 8192).unwrap();
    assert_eq!(
        db.daily_mean("archive_day_outTemp", 0, 172800).unwrap(),
        Some(27.5)
    );
    assert_eq!(
        db.daily_mean("archive_day_outTemp", 0, 86400).unwrap(),
        Some(20.0)
    );
    assert_eq!(
        db.aggregate(
            "archive_day_outTemp",
            "min",
            Span {
                start: 0,
                stop: 86400,
                daily: true
            }
        )
        .unwrap()
        .min,
        Some(-999.5)
    );
    db.export_sqlite(&restored).unwrap();
    let restore = Connection::open(restored).unwrap();
    for table in ["archive", "archive_day_outTemp", "archive_day__metadata"] {
        assert_eq!(table_rows(&connection, table), table_rows(&restore, table));
    }
    assert_eq!(
        restore
            .query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
}

#[test]
fn rejects_truncated_or_corrupt_files_and_never_overwrites() {
    let dir = tempfile::tempdir().unwrap();
    let sqlite = dir.path().join("source.sdb");
    let own = dir.path().join("archive.wwdb");
    let _connection = source(&sqlite);
    let before = fs::read(&sqlite).unwrap();
    import_sqlite(&sqlite, &own, "archive").unwrap();
    assert_eq!(before, fs::read(&sqlite).unwrap());
    let original = fs::read(&own).unwrap();
    assert!(import_sqlite(&sqlite, &own, "archive").is_err());
    assert_eq!(original, fs::read(&own).unwrap());
    let bad = dir.path().join("bad.wwdb");
    for length in [0, 10, original.len() - 1] {
        fs::write(&bad, &original[..length]).unwrap();
        assert!(Database::open(&bad, 0).is_err());
    }
    let mut corrupt = original.clone();
    let last = corrupt.len() - 1;
    corrupt[last] ^= 1;
    fs::write(&bad, &corrupt).unwrap();
    assert!(Database::open(&bad, 0).is_err());
    corrupt = original;
    corrupt[28] ^= 1;
    fs::write(&bad, corrupt).unwrap();
    let mut db = Database::open(&bad, 0).unwrap();
    let error = db
        .aggregate(
            "archive",
            "usUnits",
            Span {
                start: -900_000,
                stop: -899_000,
                daily: false,
            },
        )
        .unwrap_err();
    assert!(error.to_string().contains("checksum"), "{error}");
    assert!(
        Database::open(&own, 0)
            .unwrap()
            .export_sqlite(&sqlite)
            .is_err()
    );
    assert_eq!(before, fs::read(&sqlite).unwrap());
}

#[test]
fn empty_archive_and_failed_import_leave_no_partial_database() {
    let dir = tempfile::tempdir().unwrap();
    let sqlite = dir.path().join("source.sdb");
    let own = dir.path().join("archive.wwdb");
    let connection = Connection::open(&sqlite).unwrap();
    connection
        .execute_batch("CREATE TABLE archive(dateTime INTEGER PRIMARY KEY, outTemp REAL)")
        .unwrap();
    import_sqlite(&sqlite, &own, "archive").unwrap();
    let mut db = Database::open(&own, 0).unwrap();
    assert_eq!(db.info().rows, 0);
    assert_eq!(
        db.aggregate(
            "archive",
            "outTemp",
            Span {
                start: 0,
                stop: 10,
                daily: false
            }
        )
        .unwrap(),
        Stats::default()
    );
    connection
        .execute("INSERT INTO archive VALUES (300, 'bad measurement')", [])
        .unwrap();
    let failed = dir.path().join("failed.wwdb");
    assert!(import_sqlite(&sqlite, &failed, "archive").is_err());
    assert!(!failed.exists());
}

#[test]
fn imports_committed_wal_rows_without_checkpointing_the_source() {
    let dir = tempfile::tempdir().unwrap();
    let sqlite = dir.path().join("source.sdb");
    let own = dir.path().join("archive.wwdb");
    let connection = Connection::open(&sqlite).unwrap();
    connection
        .execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
        CREATE TABLE archive(dateTime INTEGER PRIMARY KEY, outTemp REAL);
        INSERT INTO archive VALUES (300, 0.0), (600, -2.0);",
        )
        .unwrap();
    let wal_path = sqlite.with_extension("sdb-wal");
    let wal_before = fs::read(&wal_path).unwrap();
    import_sqlite(&sqlite, &own, "archive").unwrap();
    let mut db = Database::open(&own, 0).unwrap();
    assert_eq!(
        db.aggregate(
            "archive",
            "outTemp",
            Span {
                start: 0,
                stop: 600,
                daily: false
            }
        )
        .unwrap()
        .count,
        2
    );
    assert_eq!(wal_before, fs::read(wal_path).unwrap());
}

#[test]
fn benchmark_cli_checks_both_engines_with_no_column_cache() {
    let dir = tempfile::tempdir().unwrap();
    let sqlite = dir.path().join("source.sdb");
    let own = dir.path().join("archive.wwdb");
    let _connection = source(&sqlite);
    import_sqlite(&sqlite, &own, "archive").unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_weewx-db"))
        .arg("bench")
        .arg(&sqlite)
        .arg(&own)
        .args(["--cache-mib", "0", "--rounds", "1"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["cases"].as_array().unwrap().len(), 5);
    assert_eq!(report["custom_cache_resident_bytes"], 0);
    assert!(
        report["cases"]
            .as_array()
            .unwrap()
            .iter()
            .all(|case| case["verified_answers"] == 3)
    );
}

#[test]
fn sum_retains_small_values_when_large_positive_and_negative_values_cancel() {
    let dir = tempfile::tempdir().unwrap();
    let sqlite = dir.path().join("source.sdb");
    let own = dir.path().join("archive.wwdb");
    let connection = Connection::open(&sqlite).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE archive(dateTime INTEGER PRIMARY KEY, outTemp REAL);
        INSERT INTO archive VALUES (300, 1e16), (600, 1.0), (900, -1e16);",
        )
        .unwrap();
    import_sqlite(&sqlite, &own, "archive").unwrap();
    let span = Span {
        start: 0,
        stop: 900,
        daily: false,
    };
    let expected = reference(&connection, "outTemp", span);
    assert_eq!(expected.sum, Some(1.0));
    let mut db = Database::open(&own, 0).unwrap();
    close(
        &db.aggregate("archive", "outTemp", span).unwrap(),
        &expected,
    );
    close(
        &db.buckets("archive", "outTemp", span, 900).unwrap()[0],
        &expected,
    );
}
