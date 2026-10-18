//! SQL parsing and joins use SQLite; column access, writes and aggregates use Rust.

mod fast;
mod functions;
pub mod cursor;

use crate::store::{RowCursor, Store};
use crate::*;
use std::borrow::Cow;
use std::ffi::{CStr, CString, c_int};
use std::sync::{Mutex, MutexGuard};
use std::cell::RefCell;
use rusqlite::vtab::{self, VTab, CreateVTab, UpdateVTab, VTabCursor};
use sqlparser::ast::{Statement, AlterTableOperation};
use sqlparser::dialect::SQLiteDialect;
use sqlparser::parser::Parser;

type Shared = Arc<Mutex<Store>>;

fn error(error: impl std::fmt::Display) -> rusqlite::Error {
    let text = error.to_string();
    let code = if text.contains("constraint failed") { rusqlite::ffi::SQLITE_CONSTRAINT } else { rusqlite::ffi::SQLITE_ERROR };
    rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), Some(text))
}

pub(crate) fn guard(shared: &Shared) -> rusqlite::Result<MutexGuard<'_, Store>> {
    shared.lock().map_err(|_| error("database mutex poisoned"))
}

// SQLite requires its C base struct to be the first member of each virtual
// table and cursor. All callbacks access the shared engine under its mutex.
#[repr(C)]
struct ArchiveTable {
    base: vtab::sqlite3_vtab,
    store: Shared,
    table: String,
    time_index: usize,
    connection: *mut rusqlite::ffi::sqlite3,
}

unsafe impl<'vtab> VTab<'vtab> for ArchiveTable {
    type Aux = Shared;
    type Cursor = ArchiveCursor;

    fn connect(db: &mut vtab::VTabConnection, aux: Option<&Shared>, _: &[u8], _: &[u8], table: &[u8], _: &[&[u8]]) -> rusqlite::Result<(Cow<'static, CStr>, Self)> {
        db.config(vtab::VTabConfig::ConstraintSupport)?;
        // This handle belongs to the connection that owns the virtual table.
        // It is used only during that connection's serialized write callbacks.
        let connection = unsafe { db.handle() };
        let store = aux.ok_or_else(|| error("missing engine"))?.clone();
        let table = std::str::from_utf8(table).map_err(error)?.to_owned();
        let columns = guard(&store)?.columns(&table).map_err(error)?;
        let definition = format!("CREATE TABLE x ({})", columns.iter().map(|c| format!("{} {}{}{}", quote(&c.name),
            if c.kind == Kind::Integer { "INTEGER" } else { "REAL" }, if c.not_null { " NOT NULL" } else { "" },
            c.default.as_ref().map_or_else(String::new,|v| format!(" DEFAULT {}",v.json())))).collect::<Vec<_>>().join(","));
        let time_index = columns.iter().position(|c| c.name == "dateTime").unwrap();
        Ok((Cow::Owned(CString::new(definition).map_err(error)?), Self { base: Default::default(), store, table, time_index, connection }))
    }

    fn best_index(&self, info: &mut vtab::IndexInfo) -> rusqlite::Result<bool> {
        let mut plan = Vec::new();
        let mut argument = 0;
        for (constraint, mut usage) in info.constraints_and_usages() {
            if !constraint.is_usable() { continue; }
            let op = match constraint.operator() {
                vtab::IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_EQ => "eq",
                vtab::IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_GT => "gt",
                vtab::IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_GE => "ge",
                vtab::IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_LT => "lt",
                vtab::IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_LE => "le",
                vtab::IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_ISNULL => "null",
                vtab::IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_ISNOTNULL => "notnull",
                _ => continue,
            };
            if constraint.column() >= -1 {
                if op != "null" && op != "notnull" { argument += 1; usage.set_argv_index(argument); }
                // SQLite rechecks affinity, non-integer arguments and NULLs.
                plan.push((constraint.column(),op.to_owned()));
            }
        }
        info.set_idx_str(&serde_json::to_string(&plan).map_err(error)?);
        let point = plan.iter().any(|(ci,p)| (*ci == self.time_index as i32 || *ci == -1) && p == "eq");
        info.set_estimated_cost(if point { 1.0 } else if !plan.is_empty() { 100.0 } else { 1_000_000.0 });
        info.set_estimated_rows(if point { 1 } else { 10000 });
        if info.num_of_order_by() == 1 && info.order_bys().all(|o| o.column() == self.time_index as i32 && !o.is_order_by_desc()) {
            info.set_order_by_consumed(true);
        }
        Ok(true)
    }

    fn open(&mut self) -> rusqlite::Result<ArchiveCursor> {
        Ok(ArchiveCursor { base: Default::default(), store: self.store.clone(), table: self.table.clone(), cursor: RefCell::new(None), eof: true })
    }
}

impl<'vtab> CreateVTab<'vtab> for ArchiveTable { const KIND: vtab::VTabKind = vtab::VTabKind::Default; }

impl<'vtab> UpdateVTab<'vtab> for ArchiveTable {
    fn delete(&mut self, value: ValueRef<'_>) -> rusqlite::Result<()> {
        let ValueRef::Integer(time) = value else { return Err(error("invalid rowid")); };
        guard(&self.store)?.delete(&self.table, time).map_err(error)
    }
    fn insert(&mut self, args: &vtab::Inserts<'_>) -> rusqlite::Result<i64> {
        let values = args.iter().skip(2).map(Cell::read).collect::<Result<Vec<_>>>().map_err(error)?;
        let replace = unsafe { args.on_conflict(self.connection) } == vtab::ConflictMode::Replace;
        guard(&self.store)?.put(&self.table, values, replace).map_err(error)
    }
    fn update(&mut self, args: &vtab::Updates<'_>) -> rusqlite::Result<()> {
        let old: i64 = args.get(0)?;
        let values = args.iter().skip(2).map(Cell::read).collect::<Result<Vec<_>>>().map_err(error)?;
        let mut store = guard(&self.store)?;
        let time_index = store.columns(&self.table).map_err(error)?.iter().position(|c| c.name == "dateTime").unwrap();
        let new_time = match values[time_index] { Cell::Integer(n) => n, _ => return Err(error("invalid dateTime")) };
        if old != new_time {
            let replace = unsafe { args.on_conflict(self.connection) } == vtab::ConflictMode::Replace;
            ensure_sql(replace || store.get(&self.table, new_time).map_err(error)?.is_none(), "UNIQUE constraint failed: dateTime")?;
            store.delete(&self.table, old).map_err(error)?;
        }
        store.put(&self.table, values, true).map_err(error)?;
        Ok(())
    }
}

fn ensure_sql(condition: bool, text: &str) -> rusqlite::Result<()> { if condition { Ok(()) } else { Err(error(text)) } }

#[repr(C)]
struct ArchiveCursor {
    base: vtab::sqlite3_vtab_cursor,
    store: Shared,
    table: String,
    cursor: RefCell<Option<RowCursor>>,
    eof: bool,
}

unsafe impl VTabCursor for ArchiveCursor {
    fn filter(&mut self, _: c_int, plan: Option<&str>, args: &vtab::Filters<'_>) -> rusqlite::Result<()> {
        let mut lower = None;
        let mut upper = None;
        let mut store = guard(&self.store)?;
        let columns = store.columns(&self.table).map_err(error)?;
        let ts = columns.iter().position(|c|c.name == "dateTime").unwrap();
        let plan: Vec<(i32,String)> = serde_json::from_str(plan.unwrap_or("[]")).map_err(error)?;
        let mut argument = 0;
        let mut pruning = Vec::new();
        for (ci,op) in plan {
            let value = if op == "null" || op == "notnull" { Value::Null } else {
                let value = args.get::<Value>(argument)?; argument += 1; value
            };
            if (ci == ts as i32 || ci == -1) && let Value::Integer(time) = value {
                match op.as_str() {
                    "eq" => { tighten_lower(&mut lower, time, true); tighten_upper(&mut upper, time, true); },
                    "gt" => tighten_lower(&mut lower,time,false), "ge" => tighten_lower(&mut lower,time,true),
                    "lt" => tighten_upper(&mut upper,time,false), "le" => tighten_upper(&mut upper,time,true), _ => (),
                }
            }
            if ci >= 0 && (columns[ci as usize].kind == Kind::Real || op == "null" || op == "notnull") {
                let numeric = match value { Value::Integer(n) => Some(n as f64), Value::Real(n) => Some(n), _ => None };
                pruning.push((ci as usize,op,numeric));
            }
        }
        let mut cursor = RowCursor::new_bounds(&store, &self.table, lower, upper).map_err(error)?;
        cursor.pruning = pruning;
        self.eof = !cursor.advance(&mut store).map_err(error)?;
        *self.cursor.get_mut() = Some(cursor);
        Ok(())
    }
    fn next(&mut self) -> rusqlite::Result<()> {
        self.eof = !self.cursor.get_mut().as_mut().ok_or_else(|| error("uninitialized cursor"))?.advance(&mut *guard(&self.store)?).map_err(error)?;
        Ok(())
    }
    fn eof(&self) -> bool { self.eof }
    fn column(&self, ctx: &mut vtab::Context, index: c_int) -> rusqlite::Result<()> {
        // SQLite's immutable callback still needs to cache projected columns.
        // Cursor-local interior mutability keeps the C layout and Rust aliases valid.
        let value = self.cursor.borrow_mut().as_mut().ok_or_else(|| error("uninitialized cursor"))?.cell(&mut *guard(&self.store)?, index as usize).map_err(error)?;
        ctx.set_result(&value.value())
    }
    fn rowid(&self) -> rusqlite::Result<i64> { Ok(self.cursor.borrow().as_ref().ok_or_else(|| error("uninitialized cursor"))?.time) }
}

pub(crate) fn tighten_lower(bound: &mut Option<(i64,bool)>, time: i64, inclusive: bool) {
    if bound.is_none_or(|(t,i)| time > t || (time == t && !inclusive && i)) { *bound = Some((time,inclusive)); }
}
pub(crate) fn tighten_upper(bound: &mut Option<(i64,bool)>, time: i64, inclusive: bool) {
    if bound.is_none_or(|(t,i)| time < t || (time == t && !inclusive && i)) { *bound = Some((time,inclusive)); }
}

#[derive(Debug, Serialize)]
pub struct QueryResult {
    pub columns: Vec<String>,
    #[serde(skip)]
    pub rows: Vec<Vec<Cell>>,
    pub changes: usize,
    pub engine: String,
}

impl QueryResult {
    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({"columns": self.columns, "rows": self.rows.iter().map(|r| r.iter().map(Cell::json).collect::<Vec<_>>()).collect::<Vec<_>>(), "changes": self.changes, "engine": self.engine})
    }
}

pub struct Session {
    pub(crate) connection: Connection,
    pub(crate) store: Shared,
    explicit: bool,
    metadata_dirty: bool,
    pub(crate) active_cursors: usize,
    pub(crate) closed: bool,
}

impl Session {
    pub fn open(path: &Path, cache_bytes: usize) -> Result<Self> {
        let store = Arc::new(Mutex::new(Store::open(path, cache_bytes)?));
        let connection = Self::frontend(&store)?;
        Ok(Self { connection, store, explicit: false, metadata_dirty: false, active_cursors:0, closed:false })
    }

    fn frontend(shared: &Shared) -> Result<Connection> {
        let connection = Connection::open_in_memory()?;
        const MODULE: vtab::Module<ArchiveTable> = vtab::Module::update_module();
        connection.create_module(c"weewx", &MODULE, Some(shared.clone()))?;
        functions::register(&connection,guard(shared)?.workspace_bytes)?;
        connection.execute_batch("PRAGMA temp_store=FILE")?;
        let manifest = guard(shared)?.snapshot.manifest.clone();
        for table in &manifest.tables {
            connection.execute_batch(&format!("CREATE VIRTUAL TABLE {} USING weewx", quote(&table.name)))?;
        }
        for table in &manifest.metadata {
            connection.execute_batch(&table.sql)?;
            for row in &table.rows {
                connection.execute(&format!("INSERT INTO {} VALUES ({})", quote(&table.name), vec!["?"; row.len()].join(",")), params_from_iter(row.iter().map(Cell::value)))?;
            }
        }
        Ok(connection)
    }

    pub(crate) fn refresh(&mut self) -> Result<()> {
        ensure!(!self.closed,"connection is closed");
        if self.explicit || self.active_cursors > 0 { return Ok(()); }
        let before = { let store = guard(&self.store)?; (store.lsn(),store.snapshot.manifest.id.clone()) };
        guard(&self.store)?.refresh()?;
        let changed = { let store = guard(&self.store)?; (store.lsn(),store.snapshot.manifest.id.clone()) != before };
        if changed { self.connection = Self::frontend(&self.store)?; }
        Ok(())
    }

    pub fn execute(&mut self, sql: &str, parameters: &[Cell]) -> Result<QueryResult> {
        self.refresh()?;
        ensure!(statements(sql)?.len()==1,"execute accepts one SQL statement");
        let parsed = if simple_write(sql) { None } else { Parser::parse_sql(&SQLiteDialect {},sql).ok() };
        if let Some(statements) = &parsed { ensure!(statements.len() == 1,"execute accepts one statement; use batch or explicit transactions"); }
        ensure!(!parsed.as_ref().and_then(|s|s.first()).is_some_and(|s|matches!(s,
            Statement::Savepoint {..}|Statement::ReleaseSavepoint {..}|Statement::Rollback {savepoint:Some(_),..})),"SQL savepoints are not supported");
        let trimmed = sql.trim().trim_end_matches(';').trim();
        if matches!(parsed.as_ref().and_then(|s|s.first()),Some(Statement::StartTransaction {..})) {
            ensure!(!self.explicit, "transaction already active");
            ensure!(self.active_cursors == 0,"close streaming read cursors before writing");
            guard(&self.store)?.begin()?;
            self.connection.execute_batch("BEGIN")?;
            self.explicit = true;
            return Ok(empty());
        }
        if matches!(parsed.as_ref().and_then(|s|s.first()),Some(Statement::Commit {..})) {
            ensure!(self.explicit, "no active transaction");
            self.commit()?;
            return Ok(empty());
        }
        if matches!(parsed.as_ref().and_then(|s|s.first()),Some(Statement::Rollback {savepoint:None,..})) {
            ensure!(self.explicit, "no active transaction");
            self.connection.execute_batch("ROLLBACK")?;
            guard(&self.store)?.rollback()?;
            self.explicit = false; self.metadata_dirty = false;
            self.connection = Self::frontend(&self.store)?;
            return Ok(empty());
        }
        let ddl = parsed.as_ref().and_then(|s| s.first()).is_some_and(|s| matches!(s, Statement::CreateTable(_) | Statement::AlterTable {..} | Statement::Drop {..}));
        let readonly = if ddl { false } else { self.connection.prepare_cached(trimmed)?.readonly() };
        let automatic = !readonly && !self.explicit;
        if !readonly { ensure!(self.active_cursors == 0,"close streaming read cursors before writing"); }
        if automatic { guard(&self.store)?.begin()?; self.connection.execute_batch("BEGIN")?; }
        let point = guard(&self.store)?.savepoint();
        let result = if ddl { self.ddl(parsed.as_ref().unwrap().first().unwrap(), trimmed) }
            else { self.query(trimmed, parameters, parsed.as_ref().and_then(|s| s.first())) };
        match result {
            Ok(result) => {
                if !readonly { self.metadata_dirty = true; }
                if automatic { self.commit()?; }
                Ok(result)
            }
            Err(error) => {
                if automatic {
                    let _ = self.connection.execute_batch("ROLLBACK");
                    guard(&self.store)?.rollback()?;
                    self.connection = Self::frontend(&self.store)?;
                } else if self.explicit { guard(&self.store)?.restore_statement(point)?; }
                Err(error)
            }
        }
    }

    fn commit(&mut self) -> Result<()> {
        if self.metadata_dirty {
            let definitions = guard(&self.store)?.snapshot.manifest.metadata.clone();
            for table in definitions {
                let mut query = self.connection.prepare(&format!("SELECT * FROM {}", quote(&table.name)))?;
                let count = query.column_count();
                let mut rows = query.query([])?;
                let mut data = Vec::new();
                while let Some(row) = rows.next()? {
                    ensure!(data.len() < 4096, "metadata row limit exceeded");
                    data.push((0..count).map(|i| Cell::read(row.get_ref(i)?)).collect::<Result<Vec<_>>>()?);
                }
                guard(&self.store)?.replace_metadata(&table.name, &table.sql, data)?;
            }
        }
        self.connection.execute_batch("COMMIT")?;
        let committed = guard(&self.store)?.commit();
        if let Err(error) = committed {
            guard(&self.store)?.rollback()?;
            self.explicit = false; self.metadata_dirty = false;
            self.connection = Self::frontend(&self.store)?;
            return Err(error);
        }
        self.explicit = false; self.metadata_dirty = false;
        Ok(())
    }

    fn query(&self, sql: &str, parameters: &[Cell], statement: Option<&Statement>) -> Result<QueryResult> {
        let mut prepared = self.connection.prepare_cached(sql)?;
        ensure!(prepared.parameter_count() == parameters.len(), "incorrect number of SQL bindings");
        let columns = prepared.column_names().into_iter().map(str::to_owned).collect::<Vec<_>>();
        if prepared.readonly() && let Some(statement) = statement
            && let Some(rows) = fast::query(&mut *guard(&self.store)?, statement, parameters)? {
            return Ok(QueryResult { columns, rows, changes: 0, engine: "rust-native".to_owned() });
        }
        let mut result = QueryResult { columns, rows: vec![], changes: 0, engine: "sql-rust-storage".to_owned() };
        if prepared.column_count() == 0 {
            result.changes = prepared.execute(params_from_iter(parameters.iter().map(Cell::value)))?;
        } else {
            let count = prepared.column_count();
            let mut rows = prepared.query(params_from_iter(parameters.iter().map(Cell::value)))?;
            while let Some(row) = rows.next()? {
                ensure!((result.rows.len() + 1).saturating_mul(count).saturating_mul(std::mem::size_of::<Cell>()) <= guard(&self.store)?.workspace_bytes, "result workspace exceeded; increase workspace_mib or use the streaming CLI");
                result.rows.push((0..count).map(|i| Cell::read(row.get_ref(i)?)).collect::<Result<Vec<_>>>()?);
            }
        }
        Ok(result)
    }

    fn ddl(&mut self, statement: &Statement, sql: &str) -> Result<QueryResult> {
        match statement {
            Statement::CreateTable(create) => {
                let name = create.name.to_string();
                let name = vtab::dequote(&name).into_owned();
                if create.if_not_exists && guard(&self.store)?.tables().contains(&name) { return Ok(empty()); }
                let temp = Connection::open_in_memory()?;
                temp.execute_batch(sql)?;
                let columns = schema_columns(&temp, &name)?;
                if columns.iter().any(|c| c.0 == "dateTime") {
                    let columns = numeric_columns(&temp, &name)?;
                    guard(&self.store)?.create_table(&name, sql, columns)?;
                    self.connection.execute_batch(&format!("CREATE VIRTUAL TABLE {} USING weewx", quote(&name)))?;
                } else {
                    guard(&self.store)?.replace_metadata(&name, sql, vec![])?;
                    self.connection.execute_batch(sql)?;
                }
            }
            Statement::AlterTable(alter) => {
                let name = vtab::dequote(&alter.name.to_string()).into_owned();
                ensure!(alter.operations.len() == 1, "one schema operation per statement is supported");
                let AlterTableOperation::AddColumn { column_def, .. } = &alter.operations[0] else { bail!("supported schema operation is ADD COLUMN") };
                let mut store = guard(&self.store)?;
                let ti = store.table_index(&name)?;
                let temp = Connection::open_in_memory()?;
                temp.execute_batch(&store.snapshot.manifest.tables[ti].sql)?;
                temp.execute_batch(sql)?;
                let columns = numeric_columns(&temp, &name)?;
                let column = columns.into_iter().find(|c| c.name == column_def.name.value).context("new column missing")?;
                let new_sql: String = temp.query_row("SELECT sql FROM sqlite_master WHERE name=?", [&name], |r| r.get(0))?;
                store.add_column(&name, column, &new_sql)?;
                drop(store);
                self.connection.execute_batch(&format!("DROP TABLE {}; CREATE VIRTUAL TABLE {} USING weewx", quote(&name), quote(&name)))?;
            }
            Statement::Drop { names, if_exists, .. } => {
                for name in names {
                    let name = vtab::dequote(&name.to_string()).into_owned();
                    if !guard(&self.store)?.tables().contains(&name) && *if_exists { continue; }
                    guard(&self.store)?.drop_table(&name)?;
                    self.connection.execute_batch(&format!("DROP TABLE {}", quote(&name)))?;
                }
            }
            _ => bail!("unsupported schema statement"),
        }
        Ok(empty())
    }

    pub fn ensure_read_only(&self, sql: &str) -> Result<()> {
        ensure!(self.connection.prepare(sql)?.readonly(), "statement is not read-only");
        Ok(())
    }

    /// The initial callback supplies column names with no row, including for
    /// an empty result. Historical rows are then delivered one at a time.
    pub fn stream(&mut self, sql: &str, parameters: &[Cell], mut output: impl FnMut(&[String],Option<&[Cell]>) -> Result<()>) -> Result<()> {
        self.refresh()?;
        if managed(sql)? {
            let result = self.execute(sql,parameters)?;
            output(&result.columns,None)?;
            for row in &result.rows { output(&result.columns,Some(row))?; }
            return Ok(());
        }
        let readonly = self.connection.prepare(sql)?.readonly();
        if !readonly {
            let result = self.execute(sql,parameters)?;
            output(&result.columns,None)?;
            for row in &result.rows { output(&result.columns,Some(row))?; }
            return Ok(());
        }
        let mut prepared = self.connection.prepare_cached(sql)?;
        ensure!(prepared.parameter_count() == parameters.len(), "incorrect number of SQL bindings");
        let columns = prepared.column_names().into_iter().map(str::to_owned).collect::<Vec<_>>();
        output(&columns,None)?;
        let parsed = Parser::parse_sql(&SQLiteDialect {},sql).ok();
        if let Some(statement) = parsed.as_ref().and_then(|s|s.first())
            && let Some(rows) = fast::query(&mut *guard(&self.store)?,statement,parameters)? {
            for row in &rows { output(&columns,Some(row))?; }
            return Ok(());
        }
        let count = columns.len();
        let mut rows = prepared.query(params_from_iter(parameters.iter().map(Cell::value)))?;
        while let Some(row) = rows.next()? {
            let row = (0..count).map(|i|Cell::read(row.get_ref(i)?)).collect::<Result<Vec<_>>>()?;
            output(&columns,Some(&row))?;
        }
        Ok(())
    }

    pub fn columns(&self, table: &str) -> Result<Vec<Column>> { guard(&self.store)?.columns(table) }
    pub fn info(&self) -> Result<crate::store::StoreInfo> { guard(&self.store)?.info() }

    pub fn set_workspace(&mut self, bytes: usize) -> Result<()> {
        ensure!(self.active_cursors == 0,"close streaming cursors before changing the workspace");
        guard(&self.store)?.set_workspace(bytes)?;
        functions::register(&self.connection,bytes)?;
        Ok(())
    }

    pub fn in_transaction(&self) -> bool { self.explicit }
    pub fn set_sync_mode(&mut self, mode: crate::store::SyncMode) -> Result<()> { guard(&self.store)?.set_sync_mode(mode) }
    pub fn flush(&self) -> Result<()> { guard(&self.store)?.flush() }

    pub fn append_json(&mut self, table: &str, row: &serde_json::Map<String,serde_json::Value>, replace: bool) -> Result<i64> {
        ensure!(self.explicit,"append needs an explicit transaction");
        guard(&self.store)?.put_json(table,row,replace)
    }

    pub fn checkpoint(&mut self, rebuild: bool) -> Result<()> {
        ensure!(!self.explicit, "checkpoint cannot run in a transaction");
        ensure!(self.active_cursors == 0,"close streaming read cursors before checkpointing");
        guard(&self.store)?.checkpoint(rebuild)?;
        self.connection = Self::frontend(&self.store)?;
        Ok(())
    }

    pub fn close(&mut self) -> Result<()> {
        if self.explicit {
            self.connection.execute_batch("ROLLBACK")?;
            guard(&self.store)?.rollback()?;
            self.explicit = false;
        }
        self.flush()?;
        self.closed = true;
        Ok(())
    }
}

fn managed(sql: &str) -> Result<bool> {
    ensure!(statements(sql)?.len() == 1,"execute accepts one SQL statement");
    Ok(["CREATE","ALTER","DROP","BEGIN","COMMIT","END","ROLLBACK","SAVEPOINT","RELEASE"].iter().any(|word|keyword(sql).eq_ignore_ascii_case(word)))
}

fn without_comments(mut sql: &str) -> &str {
    loop {
        sql = sql.trim_start();
        if let Some(rest) = sql.strip_prefix("--") { sql = rest.split_once('\n').map_or("",|(_,tail)|tail); }
        else if let Some(rest) = sql.strip_prefix("/*") { sql = rest.split_once("*/").map_or("",|(_,tail)|tail); }
        else { break; }
    }
    sql
}

fn keyword(sql: &str) -> &str {
    let sql = without_comments(sql);
    &sql[..sql.find(|c:char|!c.is_ascii_alphabetic()).unwrap_or(sql.len())]
}

fn simple_write(sql: &str) -> bool {
    ["INSERT","REPLACE","UPDATE","DELETE"].iter().any(|word|keyword(sql).eq_ignore_ascii_case(word))
}

pub fn statements(sql: &str) -> Result<Vec<&str>> {
    let mut parts = Vec::new();
    let mut start = 0;
    for (i,ch) in sql.char_indices() {
        if ch == ';' {
            let part = &sql[start..=i];
            let terminated = CString::new(part)?;
            // SQLite's completion parser understands quoted semicolons and
            // trigger bodies. Preserve the original SQL, including identifiers
            // such as WeeWX's interval column that other parsers may rewrite.
            if unsafe { rusqlite::ffi::sqlite3_complete(terminated.as_ptr()) } != 0 {
                parts.push(part);
                start = i+1;
            }
        }
    }
    let tail = sql[start..].trim();
    if !without_comments(tail).is_empty() { parts.push(tail); }
    Ok(parts)
}

fn empty() -> QueryResult { QueryResult { columns: vec![], rows: vec![], changes: 0, engine: "rust-journal".to_owned() } }

type SchemaColumn = (String, String, bool, Option<String>, bool);
fn schema_columns(connection: &Connection, name: &str) -> Result<Vec<SchemaColumn>> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({})", quote(name)))?;
    Ok(statement.query_map([], |row| Ok((row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn numeric_columns(connection: &Connection, name: &str) -> Result<Vec<Column>> {
    schema_columns(connection, name)?.into_iter().map(|(name, declared, not_null, default, primary)| {
        ensure!(!primary || name == "dateTime", "numeric tables use dateTime as their primary key");
        let kind = column_kind(&declared)?;
        let default = default.map(|value| connection.query_row(&format!("SELECT {value}"), [], |r| r.get::<_, Value>(0))).transpose()?;
        let default = default.map(|value| Cell::read(ValueRef::from(&value))).transpose()?.map(|cell|numeric_cell(kind,cell));
        Ok(Column { name, kind, not_null: not_null || primary, default })
    }).collect()
}
