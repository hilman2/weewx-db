use std::path::{Path, PathBuf};
use weewx_db::{Cell, Kind, Column, Span};
use weewx_db::store::{Store, sidecar};
use weewx_db::sql::Session;

fn database(path: &Path) -> Session {
    Store::create(path, "archive").unwrap();
    let mut session = Session::open(path, 8192).unwrap();
    session.execute("CREATE TABLE archive(dateTime INTEGER NOT NULL PRIMARY KEY, outTemp REAL, interval INTEGER DEFAULT 5)", &[]).unwrap();
    session
}

fn scalar(session: &mut Session, sql: &str) -> Cell {
    session.execute(sql,&[]).unwrap().rows[0][0].clone()
}

#[test]
fn sql_transactions_rollback_failed_statements_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("live.wwdb");
    let mut session = database(&path);
    session.execute("BEGIN", &[]).unwrap();
    session.execute("INSERT INTO archive VALUES (300, 12.0, 5)", &[]).unwrap();
    assert_eq!(scalar(&mut session,"SELECT COUNT(*) FROM archive"), Cell::Integer(1));
    assert!(session.execute("INSERT INTO archive VALUES (300, 99.0, 5)", &[]).is_err());
    session.execute("INSERT OR IGNORE INTO archive VALUES (300, 99.0, 5)", &[]).unwrap();
    assert_eq!(scalar(&mut session,"SELECT SUM(outTemp) FROM archive"), Cell::Real(12.0f64.to_bits()));
    session.execute("COMMIT", &[]).unwrap();
    session.execute("BEGIN", &[]).unwrap();
    session.execute("UPDATE archive SET outTemp=-5 WHERE dateTime=300", &[]).unwrap();
    session.execute("INSERT INTO archive VALUES (600, 0.0, 5)", &[]).unwrap();
    session.execute("ROLLBACK", &[]).unwrap();
    drop(session);
    let mut session = Session::open(&path,0).unwrap();
    assert_eq!(scalar(&mut session,"SELECT outTemp FROM archive WHERE dateTime=300"),Cell::Real(12.0f64.to_bits()));
    assert_eq!(scalar(&mut session,"SELECT COUNT(*) FROM archive"), Cell::Integer(1));
    session.execute("UPDATE archive SET dateTime=900, outTemp=0 WHERE dateTime=300", &[]).unwrap();
    assert_eq!(scalar(&mut session,"SELECT MIN(dateTime) FROM archive"), Cell::Integer(900));
    session.execute("INSERT OR REPLACE INTO archive VALUES (900,7,5)",&[]).unwrap();
    assert_eq!(scalar(&mut session,"SELECT SUM(outTemp) FROM archive"),Cell::Real(7.0f64.to_bits()));
}

#[test]
fn journal_tail_is_recovered_but_complete_corruption_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("live.wwdb");
    let mut session = database(&path);
    session.execute("INSERT INTO archive VALUES (300, 12.0, 5)", &[]).unwrap();
    let good = std::fs::read(sidecar(&path,".wal")).unwrap();
    session.execute("INSERT INTO archive VALUES (600, 7.0, 5)", &[]).unwrap();
    let complete = std::fs::read(sidecar(&path,".wal")).unwrap();
    drop(session);
    for tail in [1, 7, 15, complete.len()-good.len()-1] {
        std::fs::write(sidecar(&path,".wal"), &complete[..good.len()+tail]).unwrap();
        let mut session = Session::open(&path,0).unwrap();
        assert_eq!(scalar(&mut session,"SELECT COUNT(*) FROM archive"),Cell::Integer(1));
        session.execute("INSERT INTO archive VALUES (900, 3.0, 5)", &[]).unwrap();
        assert_eq!(scalar(&mut session,"SELECT COUNT(*) FROM archive"),Cell::Integer(2));
    }
    let mut corrupt = complete;
    let end = corrupt.len()-1;
    corrupt[end] ^= 1;
    std::fs::write(sidecar(&path,".wal"),corrupt).unwrap();
    assert!(Session::open(&path,0).is_err());
}

#[test]
fn schema_addition_is_durable_and_does_not_rewrite_old_payloads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("live.wwdb");
    let mut session = database(&path);
    session.execute("INSERT INTO archive VALUES (300, 12.0, 5)", &[]).unwrap();
    session.checkpoint(false).unwrap();
    let before = std::fs::read(&path).unwrap();
    session.execute("ALTER TABLE archive ADD COLUMN pm25 REAL DEFAULT 0.0", &[]).unwrap();
    assert_eq!(before,std::fs::read(&path).unwrap());
    assert_eq!(scalar(&mut session,"SELECT pm25 FROM archive WHERE dateTime=300"),Cell::Real(0.0f64.to_bits()));
    session.execute("UPDATE archive SET pm25=7.5 WHERE dateTime=300",&[]).unwrap();
    session.execute("ALTER TABLE archive ADD COLUMN humidity REAL", &[]).unwrap();
    assert_eq!(scalar(&mut session,"SELECT COUNT(humidity) FROM archive"),Cell::Integer(0));
    session.checkpoint(false).unwrap();
    let mut reopened = Session::open(&path,0).unwrap();
    assert_eq!(scalar(&mut reopened,"SELECT pm25 FROM archive WHERE dateTime=300"),Cell::Real(7.5f64.to_bits()));
    assert_eq!(scalar(&mut reopened,"SELECT COUNT(humidity) FROM archive"),Cell::Integer(0));
}

#[test]
fn checkpoint_publication_survives_an_old_journal_and_unpublished_tail() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("live.wwdb");
    let mut session = database(&path);
    session.execute("INSERT INTO archive VALUES (300, 12.0, 5)", &[]).unwrap();
    let old_wal = std::fs::read(sidecar(&path,".wal")).unwrap();
    session.checkpoint(false).unwrap();
    std::fs::write(sidecar(&path,".wal"),old_wal).unwrap();
    drop(session);
    let mut reopened = Session::open(&path,0).unwrap();
    reopened.execute("INSERT INTO archive VALUES (600, 7.0, 5)", &[]).unwrap();
    assert_eq!(scalar(&mut reopened,"SELECT COUNT(*) FROM archive"),Cell::Integer(2));
    drop(reopened);
    use std::io::Write;
    std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"unpublished checkpoint bytes").unwrap();
    let mut reopened = Session::open(&path,0).unwrap();
    assert_eq!(scalar(&mut reopened,"SELECT COUNT(*) FROM archive"),Cell::Integer(2));
}

#[test]
fn concurrent_writers_serialize_and_readers_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("live.wwdb");
    let mut reader = database(&path);
    let workers = (0..4).map(|worker| {
        let path = path.clone();
        std::thread::spawn(move || {
            let mut writer = Session::open(&path,8192).unwrap();
            writer.set_sync_mode(if worker%2 == 0 { weewx_db::store::SyncMode::Background } else { weewx_db::store::SyncMode::Always }).unwrap();
            for i in 1..=10 {
                writer.execute("INSERT INTO archive VALUES (?1, ?2, 5)", &[Cell::Integer(worker*1000+i),Cell::Real((i as f64).to_bits())]).unwrap();
            }
        })
    }).collect::<Vec<_>>();
    for worker in workers { worker.join().unwrap(); }
    assert_eq!(scalar(&mut reader,"SELECT COUNT(*) FROM archive"),Cell::Integer(40));
}

#[test]
fn complex_sql_and_rust_window_statistics_use_current_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("live.wwdb");
    let mut session = database(&path);
    session.execute("INSERT INTO archive VALUES (300,1.0,5),(600,2.0,5),(900,3.0,5),(1200,NULL,5)", &[]).unwrap();
    let result = session.execute("WITH x AS (SELECT dateTime, outTemp, LAG(outTemp) OVER (ORDER BY dateTime) AS previous FROM archive) SELECT outTemp-previous FROM x WHERE dateTime=900", &[]).unwrap();
    assert_eq!(result.rows[0][0],Cell::Real(1.0f64.to_bits()));
    assert_eq!(scalar(&mut session,"SELECT percentile_cont(outTemp,0.5) FROM archive"),Cell::Real(2.0f64.to_bits()));
    assert_eq!(scalar(&mut session,"SELECT variance_pop(outTemp) FROM archive"),Cell::Real((2.0f64/3.0).to_bits()));
    let result = session.execute("SELECT avg(outTemp) OVER (ORDER BY dateTime ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM archive", &[]).unwrap();
    assert_eq!(result.rows.iter().map(|r|r[0].numeric()).collect::<Vec<_>>(),vec![Some(1.0),Some(1.5),Some(2.5),Some(3.0)]);
    let result = session.execute("SELECT time_bucket(dateTime,600),SUM(outTemp) FROM archive GROUP BY 1 ORDER BY 1", &[]).unwrap();
    assert_eq!(result.rows.len(),3);
    assert_eq!(scalar(&mut session,"SELECT COUNT(*)/COUNT(*) FROM archive"),Cell::Integer(1));
    assert!(session.execute("SELECT SUM(outTemp) FROM archive", &[Cell::Integer(1)]).is_err());
}

#[test]
fn store_api_preserves_pending_corrections_and_schema_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let path: PathBuf = dir.path().join("live.wwdb");
    drop(database(&path));
    let mut store = Store::open(&path,0).unwrap();
    store.begin().unwrap();
    store.put("archive",vec![Cell::Integer(300),Cell::Real(12.0f64.to_bits()),Cell::Integer(5)],false).unwrap();
    store.commit().unwrap();
    store.checkpoint(false).unwrap();
    store.begin().unwrap();
    store.put("archive",vec![Cell::Integer(300),Cell::Real(8.0f64.to_bits()),Cell::Integer(5)],true).unwrap();
    store.add_column("archive",Column {name:"new_sensor".into(),kind:Kind::Real,not_null:false,default:None},"CREATE TABLE archive(dateTime INTEGER PRIMARY KEY,outTemp REAL,interval INTEGER,new_sensor REAL)").unwrap();
    store.commit().unwrap();
    let stats = store.aggregate("archive","outTemp",Span {start:0,stop:600,daily:false}).unwrap();
    assert_eq!(stats.count,1); assert_eq!(stats.sum,Some(8.0));
    store.checkpoint(false).unwrap();
    assert_eq!(Store::open(&path,0).unwrap().get("archive",300).unwrap().unwrap()[3],Cell::Null);
}

#[test]
fn grouped_native_queries_match_sql_with_corrections_gaps_and_negative_times() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("groups.wwdb");
    let mut session = database(&path);
    session.execute("INSERT INTO archive VALUES (-601,1,5),(-600,2,5),(-1,NULL,5),(0,4,5),(599,8,5),(600,16,5),(1800,NULL,5)",&[]).unwrap();
    session.checkpoint(false).unwrap();
    session.execute("UPDATE archive SET outTemp=3 WHERE dateTime=-600",&[]).unwrap();
    session.execute("DELETE FROM archive WHERE dateTime=599",&[]).unwrap();
    session.execute("INSERT INTO archive VALUES (601,32,5)",&[]).unwrap();
    for suffix in ["", " WHERE dateTime > -600 AND dateTime <= 600", " WHERE dateTime >= -600 AND dateTime < 600", " WHERE dateTime=1800", " WHERE dateTime > 10000"] {
        for order in ["ASC","DESC"] {
            let query = format!("SELECT time_bucket(dateTime,600) AS bucket,COUNT(*),COUNT(outTemp),SUM(outTemp),AVG(outTemp),MIN(outTemp),MAX(outTemp),SUM(outTemp)/COUNT(outTemp) FROM archive{suffix} GROUP BY bucket ORDER BY bucket {order}");
            let expected = session.execute(&format!("WITH rows AS (SELECT * FROM archive) {}",query.replace("FROM archive","FROM rows")),&[]).unwrap();
            let actual = session.execute(&query,&[]).unwrap();
            assert_eq!(actual.engine,"rust-native");
            assert_eq!(actual.rows,expected.rows,"{query}");
        }
    }
    assert_eq!(scalar(&mut session,"SELECT time_bucket(NULL,600)"),Cell::Null);
    let ambiguous = session.execute("SELECT time_bucket(dateTime,600) AS interval,COUNT(*) FROM archive GROUP BY interval",&[]).unwrap();
    assert_eq!(ambiguous.rows.len(),1);
    assert_eq!(ambiguous.rows[0][1],Cell::Integer(7));
    assert_eq!(scalar(&mut session,"SELECT wind_vec_avg(speed,dir,1) FROM (SELECT 0 AS speed,NULL AS dir UNION ALL SELECT 4,90)"),Cell::Real(2.0f64.to_bits()));
}

#[test]
fn persisted_summaries_avoid_payload_reads_and_info_counts_deletions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stats.wwdb");
    let mut session = database(&path);
    session.execute("INSERT INTO archive VALUES (0,1,5),(300,2,5),(600,3,5)",&[]).unwrap();
    session.checkpoint(false).unwrap();
    let mut store = Store::open(&path,0).unwrap();
    assert_eq!(store.aggregate("archive","outTemp",Span {start:-1,stop:600,daily:false}).unwrap().sum,Some(6.0));
    let info = store.info().unwrap();
    assert_eq!(info.snapshot.reads.column_reads,0);
    assert!(info.snapshot.reads.synopsis_hits > 0);
    session.execute("DELETE FROM archive WHERE dateTime=0 OR dateTime=600",&[]).unwrap();
    session.execute("INSERT INTO archive VALUES (-300,5,5),(900,6,5)",&[]).unwrap();
    store.refresh().unwrap();
    let info = store.info().unwrap();
    assert_eq!((info.snapshot.rows,info.snapshot.first,info.snapshot.last),(3,Some(-300),Some(900)));
    let before = session.execute("SELECT * FROM archive ORDER BY dateTime",&[]).unwrap().rows;
    store.vacuum().unwrap();
    assert_eq!(session.execute("SELECT * FROM archive ORDER BY dateTime",&[]).unwrap().rows,before);
    Store::open(&path,0).unwrap().verify().unwrap();
}

#[test]
fn comments_transactions_workspace_and_large_integers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("limits.wwdb");
    let mut session = database(&path);
    session.execute("-- begin with a comment\nBEGIN IMMEDIATE TRANSACTION",&[]).unwrap();
    assert!(session.execute("SAVEPOINT unsupported",&[]).is_err());
    assert!(session.execute("ROLLBACK TO unsupported",&[]).is_err());
    session.execute("/* schema */ ALTER TABLE archive ADD COLUMN counter INTEGER",&[]).unwrap();
    session.execute("INSERT INTO archive VALUES (-9223372036854775808,1,5,9223372036854775807)",&[]).unwrap();
    session.execute("-- abort\nROLLBACK TRANSACTION",&[]).unwrap();
    assert_eq!(session.columns("archive").unwrap().len(),3);
    session.execute("INSERT INTO archive VALUES (-9223372036854775808,1,5)",&[]).unwrap();
    assert_eq!(scalar(&mut session,"SELECT MIN(dateTime) FROM archive"),Cell::Integer(i64::MIN));
    session.execute("ALTER TABLE archive ADD COLUMN counter INTEGER",&[]).unwrap();
    session.execute("UPDATE archive SET counter=9223372036854775807",&[]).unwrap();
    assert_eq!(scalar(&mut session,"SELECT SUM(counter) FROM archive"),Cell::Integer(i64::MAX));
    session.set_workspace(65536).unwrap();
    session.execute("INSERT INTO archive VALUES (1000000,2,5,1)",&[]).unwrap();
    session.execute("UPDATE archive SET interval=5.0 WHERE dateTime=1000000",&[]).unwrap();
    assert_eq!(scalar(&mut session,"SELECT interval FROM archive WHERE dateTime=1000000"),Cell::Integer(5));
    assert!(session.execute("UPDATE archive SET interval=5.5 WHERE dateTime=1000000",&[]).is_err());
    assert_eq!(scalar(&mut session,"SELECT COUNT(OUTTEMP) FROM ARCHIVE WHERE DATETIME=1000000"),Cell::Integer(1));
    assert!(session.execute("SELECT SUM(counter) FROM archive",&[]).unwrap_err().to_string().contains("overflow"));
    session.execute("DELETE FROM archive WHERE dateTime < 0",&[]).unwrap();
    session.execute("INSERT INTO archive VALUES (0,2,5,1)",&[]).unwrap();
    assert!(session.execute("SELECT time_bucket(dateTime,1),AVG(outTemp) FROM archive GROUP BY 1",&[]).is_err());
}

#[test]
fn automatic_column_pruning_keeps_journal_values_and_new_sensors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("indices.wwdb");
    let mut session = database(&path);
    session.execute("BEGIN",&[]).unwrap();
    for n in 1..=9000 {
        session.execute("INSERT INTO archive VALUES (?, ?, 5)",&[Cell::Integer(n),Cell::Integer(n/100)]).unwrap();
    }
    session.execute("COMMIT",&[]).unwrap();
    session.checkpoint(false).unwrap();
    session.execute("UPDATE archive SET outTemp=1000 WHERE dateTime=1",&[]).unwrap();
    session.execute("ALTER TABLE archive ADD COLUMN sensor REAL",&[]).unwrap();
    session.execute("UPDATE archive SET sensor=0 WHERE dateTime=2",&[]).unwrap();
    let found = session.execute("SELECT dateTime FROM archive WHERE outTemp > 999",&[]).unwrap();
    assert_eq!(found.rows,vec![vec![Cell::Integer(1)]]);
    assert!(session.info().unwrap().snapshot.reads.skipped_blocks >= 3);
    assert_eq!(scalar(&mut session,"SELECT COUNT(*) FROM archive WHERE sensor IS NULL"),Cell::Integer(8999));
    assert_eq!(scalar(&mut session,"SELECT dateTime FROM archive WHERE sensor=0"),Cell::Integer(2));
    session.checkpoint(false).unwrap();
    assert_eq!(scalar(&mut session,"SELECT dateTime FROM archive WHERE outTemp > 999"),Cell::Integer(1));
}

#[test]
fn background_sync_recovers_only_the_last_torn_transaction() {
    use weewx_db::store::SyncMode;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relaxed.wwdb");
    let mut session = database(&path);
    session.set_sync_mode(SyncMode::Background).unwrap();
    session.execute("INSERT INTO archive VALUES (300,1,5)",&[]).unwrap();
    session.execute("INSERT INTO archive VALUES (600,2,5)",&[]).unwrap();
    let mut wal = std::fs::read(sidecar(&path,".wal")).unwrap();
    session.flush().unwrap();
    drop(session);
    let last = wal.len()-1;
    wal[last] ^= 1;
    std::fs::write(sidecar(&path,".wal"),&wal).unwrap();
    let mut restored = Session::open(&path,0).unwrap();
    assert_eq!(scalar(&mut restored,"SELECT COUNT(*) FROM archive"),Cell::Integer(1));
    restored.execute("INSERT INTO archive VALUES (900,3,5)",&[]).unwrap();
    drop(restored);
    let mut restored = Session::open(&path,0).unwrap();
    assert_eq!(scalar(&mut restored,"SELECT COUNT(*) FROM archive"),Cell::Integer(2));
    // Damage to a previous frame must still stop recovery, because another
    // commit after it means that predecessor had already been synchronized.
    let header_len = u32::from_le_bytes(wal[8..12].try_into().unwrap()) as usize;
    let schema_start = 16+header_len;
    let schema_size = (u32::from_le_bytes(wal[schema_start..schema_start+4].try_into().unwrap()) & !(1<<31)) as usize;
    let first_start = schema_start+16+schema_size;
    wal[first_start+16] ^= 1;
    std::fs::write(sidecar(&path,".wal"),wal).unwrap();
    assert!(Session::open(&path,0).is_err());
}

#[test]
fn previous_journal_format_is_readable_and_upgraded_before_writing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("previous.wwdb");
    let mut session = database(&path);
    session.execute("INSERT INTO archive VALUES (300,1,5)",&[]).unwrap();
    drop(session);
    let mut wal = std::fs::read(sidecar(&path,".wal")).unwrap();
    wal[..8].copy_from_slice(b"WWXWAL02");
    let header_len = u32::from_le_bytes(wal[8..12].try_into().unwrap()) as usize;
    let mut offset = 16+header_len;
    while offset < wal.len() {
        let size = u32::from_le_bytes(wal[offset..offset+4].try_into().unwrap()) as usize;
        let checksum = crc32fast::hash(&wal[offset+8..offset+16+size]);
        wal[offset+4..offset+8].copy_from_slice(&checksum.to_le_bytes());
        offset += 16+size;
    }
    std::fs::write(sidecar(&path,".wal"),wal).unwrap();
    let mut restored = Session::open(&path,0).unwrap();
    assert_eq!(scalar(&mut restored,"SELECT COUNT(*) FROM archive"),Cell::Integer(1));
    restored.execute("INSERT INTO archive VALUES (600,2,5)",&[]).unwrap();
    drop(restored);
    assert_eq!(&std::fs::read(sidecar(&path,".wal")).unwrap()[..8],b"WWXWAL03");
    assert_eq!(scalar(&mut Session::open(&path,0).unwrap(),"SELECT COUNT(*) FROM archive"),Cell::Integer(2));
}
