use weewx_db::{Cell,Span};
use weewx_db::computed::{Computed,Formula};
use weewx_db::sql::Session;
use weewx_db::store::Store;

#[test]
fn computes_before_aggregation_projects_columns_and_applies_corrections() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("computed.wwdb");
    Store::create(&path,"archive").unwrap();
    let mut session = Session::open(&path,0).unwrap();
    session.execute("CREATE TABLE archive(dateTime INTEGER PRIMARY KEY,usUnits INTEGER,interval INTEGER,outTemp REAL,outHumidity REAL,unused REAL)",&[]).unwrap();
    session.execute("BEGIN",&[]).unwrap();
    for n in 1..=6000 {
        session.execute("INSERT INTO archive VALUES (?,17,5,?,?,?)",&[Cell::Integer(n),
            Cell::Real((if n%2 == 0 { 30.0_f64 } else { 0.0_f64 }).to_bits()),
            Cell::Real((if n%2 == 0 { 90.0_f64 } else { 10.0_f64 }).to_bits()),Cell::Real((n as f64).to_bits())]).unwrap();
    }
    session.execute("COMMIT",&[]).unwrap();
    session.checkpoint(false).unwrap();
    let source = Computed {formula:Formula::Dewpoint,temperature:"outTemp".into(),humidity:"outHumidity".into(),unit_system:17};
    let span = Span {start:0,stop:6000,daily:false};
    let mut store = Store::open(&path,0).unwrap();
    let stats = store.computed_aggregate("archive",&source,span,true).unwrap();
    let expected = (source.value(Some(30.0),Some(90.0)).unwrap()+source.value(Some(0.0),Some(10.0)).unwrap())/2.0;
    assert!((stats.sum.unwrap()/stats.count as f64-expected).abs() < 1e-12);
    assert!((source.value(Some(15.0),Some(50.0)).unwrap()-expected).abs() > 1.0);
    assert!(store.info().unwrap().snapshot.reads.column_reads <= 8);
    session.execute("UPDATE archive SET outHumidity=NULL WHERE dateTime=2",&[]).unwrap();
    session.execute("DELETE FROM archive WHERE dateTime=1",&[]).unwrap();
    store.refresh().unwrap();
    let ranges = store.computed_ranges("archive",&source,&[(0,3000),(3000,6000)],true).unwrap();
    assert_eq!(ranges.iter().map(|s|s.count).collect::<Vec<_>>(),vec![2998,3000]);
    let expected = store.computed_aggregate("archive",&source,span,true).unwrap();
    let actual = session.execute("SELECT COUNT(dewpoint_c(outTemp,outHumidity)),AVG(dewpoint_c(outTemp,outHumidity)),MIN(dewpoint_c(outTemp,outHumidity)),MAX(dewpoint_c(outTemp,outHumidity)) FROM archive",&[]).unwrap();
    assert_eq!(actual.engine,"rust-native");
    assert_eq!(actual.rows[0][0],Cell::Integer(expected.count as i64));
    assert!((actual.rows[0][1].numeric().unwrap()-expected.sum.unwrap()/expected.count as f64).abs() < 1e-12);
    let grouped = session.execute("SELECT time_bucket(dateTime,3000),AVG(humidex_c(outTemp,outHumidity)) FROM archive GROUP BY 1",&[]).unwrap();
    let fallback = session.execute("WITH records AS (SELECT * FROM archive) SELECT time_bucket(dateTime,3000),AVG(humidex_c(outTemp,outHumidity)) FROM records GROUP BY 1",&[]).unwrap();
    assert_eq!(grouped.engine,"rust-native");
    assert_eq!(grouped.rows,fallback.rows);
    let empty = session.execute("SELECT SUM(dewpoint_c(outTemp,outHumidity)),COUNT(dewpoint_c(outTemp,outHumidity)) FROM archive WHERE dateTime > 7000",&[]).unwrap();
    assert_eq!(empty.rows,vec![vec![Cell::Null,Cell::Integer(0)]]);
    let empty = session.execute("SELECT AVG(outTemp),AVG(dewpoint_c(outTemp,outHumidity)),COUNT(dewpoint_c(outTemp,outHumidity)) FROM archive WHERE dateTime > 5000 AND dateTime <= 4000",&[]).unwrap();
    assert_eq!(empty.rows,vec![vec![Cell::Null,Cell::Null,Cell::Integer(0)]]);
}
