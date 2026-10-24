mod benchmark;
mod fixture;
mod cli;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use weewx_db::{Database, Span};

#[derive(Parser)]
#[command(version, about = "WeeWX column store, SQL and SQLite comparison")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(flatten)]
    Database(cli::Action),
    /// Import archive, daily summaries and their metadata from a read snapshot.
    Import {
        source: PathBuf,
        destination: PathBuf,
        #[arg(long, default_value = "archive")]
        table: String,
    },
    /// Export the imported tables to a new SQLite database.
    Export {
        source: PathBuf,
        destination: PathBuf,
    },
    /// Show schema, row count, timestamp bounds and cache usage.
    Info { database: PathBuf },
    /// Calculate COUNT, SUM, MIN and MAX over (start, stop].
    Query {
        database: PathBuf,
        column: String,
        #[arg(allow_negative_numbers = true)]
        start: i64,
        #[arg(allow_negative_numbers = true)]
        stop: i64,
        #[arg(long, default_value = "archive")]
        table: String,
        /// Use daily-summary bounds [start, stop).
        #[arg(long)]
        daily: bool,
        #[arg(long, default_value_t = 32)]
        cache_mib: usize,
        /// Return consecutive aggregate bins of this many seconds.
        #[arg(long)]
        bucket_seconds: Option<i64>,
    },
    /// Compare both engines, checking every answer outside the timed sections.
    Bench {
        source: PathBuf,
        database: PathBuf,
        #[arg(long, default_value_t = 32)]
        cache_mib: usize,
        #[arg(long, default_value_t = 7)]
        rounds: usize,
        #[arg(long, default_value = "outTemp")]
        column: String,
    },
    /// Generate deterministic synthetic weather data for a reproducible trial.
    Fixture {
        destination: PathBuf,
        #[arg(long, default_value_t = 10)]
        years: usize,
        #[arg(long, default_value_t = 32)]
        columns: usize,
        /// Populate this many observation columns; leave other sensors NULL.
        #[arg(long, default_value_t = 16)]
        active_columns: usize,
    },
}

fn cache_bytes(mib: usize) -> Result<usize> {
    mib.checked_mul(1024 * 1024).context("cache size overflow")
}

fn run() -> Result<()> {
    match Cli::parse().command {
        Command::Database(action) => cli::run(action)?,
        Command::Import {
            source,
            destination,
            table,
        } => {
            weewx_db::import_sqlite(&source, &destination, &table)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&Database::open(&destination, 0)?.info())?
            );
        }
        Command::Export {
            source,
            destination,
        } => weewx_db::store::Store::open_read_only(&source, 0)?.export_sqlite(&destination)?,
        Command::Info { database } => println!(
            "{}",
            serde_json::to_string_pretty(&weewx_db::store::Store::open_read_only(&database, 0)?.info()?)?
        ),
        Command::Query {
            database,
            column,
            start,
            stop,
            table,
            daily,
            cache_mib,
            bucket_seconds,
        } => {
            let mut database = weewx_db::store::Store::open(&database, cache_bytes(cache_mib)?)?;
            let span = Span { start, stop, daily };
            if let Some(seconds) = bucket_seconds {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &database.buckets(&table, &column, span, seconds)?
                    )?
                );
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&database.aggregate(&table, &column, span)?)?
                );
            }
        }
        Command::Bench {
            source,
            database,
            cache_mib,
            rounds,
            column,
        } => {
            let report =
                benchmark::run(&source, &database, cache_bytes(cache_mib)?, rounds, &column)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Command::Fixture {
            destination,
            years,
            columns,
            active_columns,
        } => fixture::create(&destination, years, columns, active_columns)?,
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}
