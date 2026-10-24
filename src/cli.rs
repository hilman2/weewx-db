use std::io::{BufRead, Write};
use std::path::PathBuf;
use anyhow::{Result, Context, ensure};
use clap::{Subcommand, ValueEnum};
use weewx_db::{Cell, quote};
use weewx_db::store::Store;
use weewx_db::sql::Session;

#[derive(Clone, Copy, ValueEnum)]
pub enum Format { Json, Jsonl, Csv, Tsv }

#[derive(Subcommand)]
pub enum Action {
    /// Create a writable archive; add further sensors with ALTER TABLE.
    Init { database: PathBuf },
    /// Append JSONL records from a file or stdin; each batch is atomic.
    Append {
        database: PathBuf,
        #[arg(default_value="-")] input: String,
        /// One JSON object, useful with xargs -I.
        #[arg(long)] row: Option<String>,
        #[arg(long,default_value="archive")] table: String,
        #[arg(long,default_value_t=200)] batch_size: usize,
        #[arg(long)] upsert: bool,
        /// Introduce unknown numeric sensors as nullable REAL columns.
        #[arg(long)] auto_add: bool,
    },
    /// Execute SQL with positional JSON parameters and machine-readable output.
    Sql {
        database: PathBuf,
        sql: Option<String>,
        #[arg(long)] file: Option<PathBuf>,
        #[arg(long="param",allow_hyphen_values=true)] parameters: Vec<String>,
        #[arg(long,value_enum,default_value="json")] format: Format,
        #[arg(long,default_value_t=32)] cache_mib: usize,
        #[arg(long,default_value_t=32)] workspace_mib: usize,
        #[arg(long)] read_only: bool,
    },
    /// Process JSONL {sql,params} requests in one persistent session.
    Batch { database: PathBuf, #[arg(long,default_value_t=32)] cache_mib: usize, #[arg(long,default_value_t=32)] workspace_mib: usize },
    /// Fold changed rows into blocks now; --optimize also recomputes block summaries.
    Checkpoint { database: PathBuf, #[arg(long)] optimize: bool },
    /// Delete replaced blocks and rebuild the file without free pages.
    Vacuum { database: PathBuf },
    /// Verify the file structure, every block checksum, timestamps and summaries.
    Check { database: PathBuf },
    /// Write a consistent copy to a new file, also while WeeWX writes.
    Backup { database: PathBuf, destination: PathBuf },
    /// Display SQL's time-index and column-index query plan.
    Explain { database: PathBuf, sql: String },
}

pub fn run(action: Action) -> Result<()> {
    match action {
        Action::Init { database } => {
            Store::create(&database,"archive")?;
            let mut session = Session::open(&database,1024*1024)?;
            session.execute("CREATE TABLE archive(dateTime INTEGER NOT NULL PRIMARY KEY, usUnits INTEGER NOT NULL DEFAULT 17, interval INTEGER NOT NULL DEFAULT 5, outTemp REAL, outHumidity REAL, barometer REAL, rain REAL, windSpeed REAL, windDir REAL, windGust REAL)",&[])?;
        }
        Action::Append { database,input,row,table,batch_size,upsert,auto_add } => {
            ensure!((1..=10000).contains(&batch_size),"batch size must be 1..10000");
            let input: Box<dyn BufRead> = if let Some(row) = row { Box::new(std::io::Cursor::new(format!("{row}\n"))) }
                else if input == "-" { Box::new(std::io::BufReader::new(std::io::stdin())) }
                else { Box::new(std::io::BufReader::new(std::fs::File::open(input)?)) };
            let mut session = Session::open(&database,1024*1024)?;
            let mut count = 0;
            let mut pending = 0;
            for (number,line) in input.lines().enumerate() {
                let line = line?;
                if line.trim().is_empty() { continue; }
                ensure!(line.len() <= 16*1024*1024,"input line is too long");
                let result = (|| -> Result<()> {
                    let value: serde_json::Value = serde_json::from_str(&line)?;
                    let row = value.as_object().context("expected a JSON object")?;
                    if pending == 0 { session.execute("BEGIN",&[])?; }
                    let columns = session.columns(&table)?;
                    if auto_add {
                        for (name,value) in row {
                            if !columns.iter().any(|c| c.name == *name) {
                                ensure!(value.is_number() || value.is_null(),"new sensor {name} must be numeric");
                                session.execute(&format!("ALTER TABLE {} ADD COLUMN {} REAL",quote(&table),quote(name)),&[])?;
                            }
                        }
                    }
                    session.append_json(&table,row,upsert)?;
                    pending += 1;
                    if pending == batch_size { session.execute("COMMIT",&[])?; count += pending; pending = 0; }
                    Ok(())
                })();
                if let Err(error) = result { let _ = session.execute("ROLLBACK",&[]); return Err(error).with_context(||format!("input line {}; {count} records already committed",number+1)); }
            }
            if pending > 0 { session.execute("COMMIT",&[])?; count += pending; }
            println!("{}",serde_json::json!({"inserted":count}));
        }
        Action::Sql { database,sql,file,parameters,format,cache_mib,workspace_mib,read_only } => {
            ensure!(!(sql.is_some() && file.is_some()),"provide SQL text or --file");
            let text = if let Some(sql) = sql { sql } else if let Some(file) = file { std::fs::read_to_string(file)? }
                else { std::io::read_to_string(std::io::stdin())? };
            let parameters = parameters.iter().map(|s| Cell::from_json(&serde_json::from_str(s)?)).collect::<Result<Vec<_>>>()?;
            let mut session = Session::open(&database,cache_bytes(cache_mib)?)?;
            session.set_workspace(cache_bytes(workspace_mib)?)?;
            let statements = weewx_db::sql::statements(&text)?;
            ensure!(statements.len() == 1 || parameters.is_empty(),"parameters require one statement");
            let mut output = std::io::BufWriter::new(std::io::stdout().lock());
            for statement in statements {
                if read_only { session.ensure_read_only(statement)?; }
                let mut first_row = true;
                session.stream(statement,&parameters,|columns,row| {
                    match row {
                        None => match format {
                            Format::Csv|Format::Tsv => separated(&mut output,&columns.iter().map(|s|Some(s.as_str())).collect::<Vec<_>>(),matches!(format,Format::Tsv))?,
                            Format::Json => output.write_all(b"[")?, _ => (),
                        },
                        Some(row) => match format {
                            Format::Json|Format::Jsonl => {
                                let object = columns.iter().zip(row).map(|(name,value)|(name.clone(),value.json())).collect::<serde_json::Map<_,_>>();
                                if matches!(format,Format::Json) && !first_row { output.write_all(b",")?; }
                                serde_json::to_writer(&mut output,&object)?;
                                if matches!(format,Format::Jsonl) { output.write_all(b"\n")?; }
                                first_row = false;
                            }
                            Format::Csv|Format::Tsv => {
                                let fields = row.iter().map(|cell|match cell {
                                    Cell::Null=>None, Cell::Text(s)=>Some(s.clone()), other=>Some(other.json().to_string()),
                                }).collect::<Vec<_>>();
                                separated(&mut output,&fields.iter().map(|s|s.as_deref()).collect::<Vec<_>>(),matches!(format,Format::Tsv))?;
                            }
                        }
                    }
                    Ok(())
                })?;
                if matches!(format,Format::Json) { output.write_all(b"]\n")?; }
            }
            output.flush()?;
        }
        Action::Batch { database,cache_mib,workspace_mib } => {
            let mut session = Session::open(&database,cache_bytes(cache_mib)?)?;
            session.set_workspace(cache_bytes(workspace_mib)?)?;
            let mut output = std::io::BufWriter::new(std::io::stdout().lock());
            for line in std::io::stdin().lock().lines() {
                let request: serde_json::Value = serde_json::from_str(&line?)?;
                let sql = request["sql"].as_str().context("sql string missing")?;
                let parameters = request.get("params").and_then(|v|v.as_array()).map(|v|v.iter().map(Cell::from_json).collect::<Result<Vec<_>>>()).transpose()?.unwrap_or_default();
                let result = session.execute(sql,&parameters)?;
                serde_json::to_writer(&mut output,&result.json())?; output.write_all(b"\n")?; output.flush()?;
            }
        }
        Action::Checkpoint { database,optimize } => Store::open(&database,1024*1024)?.checkpoint(optimize)?,
        Action::Vacuum { database } => Store::open(&database,1024*1024)?.vacuum()?,
        Action::Check { database } => { Store::open_read_only(&database,0)?.verify()?; println!("ok"); }
        Action::Backup { database,destination } => Store::open_read_only(&database,0)?.backup(&destination)?,
        Action::Explain { database,sql } => {
            let mut session = Session::open(&database,1024*1024)?;
            println!("{}",session.execute(&format!("EXPLAIN QUERY PLAN {sql}"),&[])?.json());
        }
    }
    Ok(())
}

fn cache_bytes(mib: usize) -> Result<usize> { mib.checked_mul(1024*1024).context("cache size overflow") }

fn separated(output: &mut impl Write, fields: &[Option<&str>], tsv: bool) -> Result<()> {
    let separator = if tsv { b'\t' } else { b',' };
    for (i,field) in fields.iter().enumerate() {
        if i > 0 { output.write_all(&[separator])?; }
        if let Some(field) = field {
            if field.contains([',','\t','\n','\r','"']) || field.is_empty() {
                output.write_all(b"\"")?; output.write_all(field.replace('"',"\"\"").as_bytes())?; output.write_all(b"\"")?;
            } else { output.write_all(field.as_bytes())?; }
        }
    }
    output.write_all(b"\n")?;
    Ok(())
}
