use std::fs::File;
use std::path::Path;

use anyhow::{Result, ensure};
use rusqlite::types::Value;
use rusqlite::{Connection, params_from_iter};

/// A 365-day year is a dataset size, not a calendar assumption in the engine.
pub fn create(
    destination: &Path,
    years: usize,
    columns: usize,
    active_columns: usize,
) -> Result<()> {
    ensure!(
        (1..=100).contains(&years) && (5..=512).contains(&columns),
        "years must be 1..100 and columns 5..512"
    );
    ensure!(
        (1..=columns).contains(&active_columns),
        "active columns must be 1..columns"
    );
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let output = tempfile::NamedTempFile::new_in(parent)?;
    {
        let mut connection = Connection::open(output.path())?;
        let transaction = connection.transaction()?;
        let mut names = vec![
            "outTemp".to_owned(),
            "barometer".to_owned(),
            "rain".to_owned(),
            "windSpeed".to_owned(),
            "windGust".to_owned(),
        ];
        names.extend((5..columns).map(|i| format!("extraTemp{i}")));
        transaction.execute_batch(&format!(
            "CREATE TABLE archive(dateTime INTEGER PRIMARY KEY, usUnits INTEGER NOT NULL, interval INTEGER NOT NULL, {});
             CREATE TABLE archive_day_outTemp(dateTime INTEGER PRIMARY KEY, min REAL, mintime INTEGER,
                max REAL, maxtime INTEGER, sum REAL, count INTEGER, wsum REAL, sumtime INTEGER);
             CREATE TABLE archive_day__metadata(name CHAR(20) PRIMARY KEY, value TEXT);",
            names.iter().map(|name| format!("{name} REAL")).collect::<Vec<_>>().join(",")
        ))?;
        let placeholders = vec!["?"; columns + 3].join(",");
        let mut insert =
            transaction.prepare(&format!("INSERT INTO archive VALUES ({placeholders})"))?;
        let mut insert_daily =
            transaction.prepare("INSERT INTO archive_day_outTemp VALUES (?,?,?,?,?,?,?,?,?)")?;
        let mut count = 0_i64;
        let mut total = 0.0;
        let mut minimum = f64::INFINITY;
        let mut maximum = f64::NEG_INFINITY;
        let mut mintime = 0_i64;
        let mut maxtime = 0_i64;
        let mut last = 0_i64;
        for i in 1..=years * 365 * 288 {
            let timestamp = 1_704_067_200 + i as i64 * 300;
            last = timestamp;
            let season = (i as f64 / (288.0 * 365.0) * std::f64::consts::TAU).sin();
            let diurnal = (i as f64 / 288.0 * std::f64::consts::TAU).sin();
            let temperature = 12.0 + 13.0 * season + 5.0 * diurnal;
            let mut values = vec![
                Value::Integer(timestamp),
                Value::Integer(17),
                Value::Integer(5),
            ];
            for column in 0..columns {
                let value = if column >= active_columns || (i + column * 11).is_multiple_of(37) {
                    Value::Null
                } else {
                    Value::Real(match column {
                        0 => temperature,
                        1 => 1013.0 + 15.0 * season,
                        2 => {
                            if i.is_multiple_of(113) {
                                0.4
                            } else {
                                0.0
                            }
                        }
                        3 => 3.0 + 2.0 * diurnal,
                        4 => 7.0 + 3.0 * diurnal,
                        _ => temperature + column as f64 * 0.1,
                    })
                };
                values.push(value);
            }
            if let Value::Real(value) = values[3] {
                count += 1;
                total += value;
                if value < minimum {
                    minimum = value;
                    mintime = timestamp;
                }
                if value > maximum {
                    maximum = value;
                    maxtime = timestamp;
                }
            }
            insert.execute(params_from_iter(values))?;
            if i.is_multiple_of(288) {
                // Distinct LOOP extrema catch migrations that silently rebuild
                // daily summaries from the lower-frequency archive values.
                insert_daily.execute(rusqlite::params![
                    timestamp - 86400,
                    minimum - 0.5,
                    mintime,
                    maximum + 0.5,
                    maxtime,
                    total,
                    count,
                    total * 300.0,
                    count * 300
                ])?;
                count = 0;
                total = 0.0;
                minimum = f64::INFINITY;
                maximum = f64::NEG_INFINITY;
            }
        }
        drop(insert);
        drop(insert_daily);
        transaction.execute(
            "INSERT INTO archive_day__metadata VALUES ('Version','4.0')",
            [],
        )?;
        transaction.execute(
            "INSERT INTO archive_day__metadata VALUES ('lastUpdate',?)",
            [last.to_string()],
        )?;
        transaction.commit()?;
    }
    output.as_file().sync_all()?;
    output.persist_noclobber(destination)?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}
