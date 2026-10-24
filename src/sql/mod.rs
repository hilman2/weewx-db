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

    /// The index string lists the constraints in argument order as
    /// `column:operator`, then `|` and the delivery order: empty for
    /// unordered, `asc` or `desc` by dateTime, or `top:column:descending`.
    fn best_index(&self, info: &mut vtab::IndexInfo) -> rusqlite::Result<bool> {
        let mut plan = Vec::new();
        let mut argument = 0;
        // SQLite offers LIMIT only when every WHERE term is a constraint on
        // this table, and keeps a plan with LIMIT only when every constraint
        // has an argument. Top-k evaluates each of them itself.
        let mut evaluable = true;
        let mut limited = false;
        for (constraint, mut usage) in info.constraints_and_usages() {
            if !constraint.is_usable() { evaluable = false; continue; }
            let op = match constraint.operator() {
                vtab::IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_LIMIT | vtab::IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_OFFSET => {
                    let limit = constraint.operator() == vtab::IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_LIMIT;
                    limited |= limit;
                    argument += 1; usage.set_argv_index(argument);
                    plan.push((-2, if limit { "limit" } else { "offset" }.to_owned()));
                    continue;
                }
                vtab::IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_EQ => "eq",
                vtab::IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_GT => "gt",
                vtab::IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_GE => "ge",
                vtab::IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_LT => "lt",
                vtab::IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_LE => "le",
                vtab::IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_ISNULL => "null",
                vtab::IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_ISNOTNULL => "notnull",
                vtab::IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_NE => "ne",
                vtab::IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_IS => "is",
                vtab::IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_ISNOT => "isnot",
                _ => { evaluable = false; continue },
            };
            if constraint.column() >= -1 {
                // NULL tests receive an argument too; their value is unused.
                argument += 1; usage.set_argv_index(argument);
                // SQLite rechecks affinity, non-integer arguments and NULLs.
                plan.push((constraint.column(),op.to_owned()));
            }
        }
        let time = |column: c_int| column == self.time_index as i32 || column == -1;
        let orders: Vec<(c_int, bool)> = info.order_bys().map(|o| (o.column(), o.is_order_by_desc())).collect();
        let order = match orders.as_slice() {
            [(column, false)] if time(*column) => "asc".to_owned(),
            [(column, true)] if time(*column) => "desc".to_owned(),
            [(column, descending)] | [(column, descending), (_, false)]
                if limited && evaluable && *column >= 0 && !time(*column) && orders.get(1).is_none_or(|o| time(o.0)) =>
                format!("top:{column}:{}", u8::from(*descending)),
            _ => String::new(),
        };
        if !order.is_empty() { info.set_order_by_consumed(true); }
        let point = plan.iter().any(|(ci,p)| time(*ci) && p == "eq");
        info.set_idx_str(&format!("{}|{order}", plan.iter().map(|(ci,op)| format!("{ci}:{op}")).collect::<Vec<_>>().join(",")));
        info.set_estimated_cost(if point { 1.0 } else if !plan.is_empty() { 100.0 } else { 1_000_000.0 });
        info.set_estimated_rows(if point { 1 } else { 10000 });
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
        let time_index = store.column_defs(&self.table).map_err(error)?.iter().position(|c| c.name == "dateTime").unwrap();
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
        let columns = store.column_defs(&self.table).map_err(error)?;
        let ts = columns.iter().position(|c|c.name == "dateTime").unwrap();
        let (plan, order) = plan.unwrap_or("|").split_once('|').ok_or_else(|| error("invalid index plan"))?;
        let plan = plan.split(',').filter(|s| !s.is_empty()).map(|item| {
            let (ci, op) = item.split_once(':').ok_or_else(|| error("invalid index plan"))?;
            Ok((ci.parse::<i32>().map_err(error)?, op.to_owned()))
        }).collect::<rusqlite::Result<Vec<_>>>()?;
        let mut pruning = Vec::new();
        let mut filters = Vec::new();
        let (mut limit, mut offset) = (None, 0_usize);
        // Every constraint has an argument, in plan order.
        for (argument, (ci,op)) in plan.into_iter().enumerate() {
            let value = args.get::<Value>(argument)?;
            let value = if op == "null" || op == "notnull" { Value::Null } else { value };
            if ci == -2 {
                // A negative LIMIT means no limit; SQLite applies both itself.
                let count = match value { Value::Integer(n) if n >= 0 => Some(n as usize), _ => None };
                if op == "limit" { limit = count; } else { offset = count.unwrap_or(0); }
                continue;
            }
            filters.push(crate::store::Filter { column: if ci == -1 { ts } else { ci as usize }, op: op.clone(), value: value.clone() });
            if (ci == ts as i32 || ci == -1) && let Value::Integer(time) = value {
                match op.as_str() {
                    "eq" => { tighten_lower(&mut lower, time, true); tighten_upper(&mut upper, time, true); },
                    "gt" => tighten_lower(&mut lower,time,false), "ge" => tighten_lower(&mut lower,time,true),
                    "lt" => tighten_upper(&mut upper,time,false), "le" => tighten_upper(&mut upper,time,true), _ => (),
                }
            }
            // Block summaries prune comparisons that a NULL reading fails.
            // IS and IS NOT can be true for NULL and are never pruned.
            if ci >= 0 && op != "is" && op != "isnot" && (columns[ci as usize].kind == Kind::Real || op == "null" || op == "notnull") {
                let numeric = match value { Value::Integer(n) => Some(n as f64), Value::Real(n) => Some(n), _ => None };
                pruning.push((ci as usize,op,numeric));
            }
        }
        let mut cursor = match order.split(':').collect::<Vec<_>>().as_slice() {
            ["desc"] => RowCursor::new_reverse(&store, &self.table, lower, upper).map_err(error)?,
            ["top", column, descending] => {
                let column = column.parse::<usize>().map_err(error)?;
                let descending = *descending == "1";
                let workspace = store.workspace_bytes / 32;
                let limit = limit.map(|n| n.saturating_add(offset));
                let ordered = match limit {
                    Some(limit) => store.ordered_times(&self.table, column, descending, (lower, upper), &filters, limit).map_err(error)?,
                    None => None,
                };
                // Without an exact filter or limit, order every row in range
                // and let SQLite recheck the WHERE terms.
                let times = match ordered { Some(times) => times,
                    None => store.ordered_times(&self.table, column, descending, (lower, upper), &[], workspace).map_err(error)?.unwrap_or_default() };
                ensure_sql(times.len() < workspace || limit.is_some_and(|l| times.len() <= l), "ORDER BY workspace exceeded; increase workspace_mib or add a LIMIT")?;
                RowCursor::new_times(&store, &self.table, times).map_err(error)?
            }
            _ => RowCursor::new_bounds(&store, &self.table, lower, upper).map_err(error)?,
        };
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
        let mut cursor = self.cursor.borrow_mut();
        let cursor = cursor.as_mut().ok_or_else(|| error("uninitialized cursor"))?;
        let value = match cursor.loaded_cell(index as usize) {
            Some(value) => value,
            None => cursor.cell(&mut *guard(&self.store)?, index as usize).map_err(error)?,
        };
        match value {
            Cell::Integer(n) => ctx.set_result(&n),
            Cell::Real(bits) => ctx.set_result(&f64::from_bits(bits)),
            Cell::Null => ctx.set_result(&rusqlite::types::Null),
            other => ctx.set_result(&other.value()),
        }
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
    /// Streaming statements of open cursors, by cursor id. Replacing the
    /// frontend connection finalizes them first and records the ids in
    /// `aborted`, so that their cursors report an error instead of touching
    /// a statement of a closed connection.
    pub(crate) streams: HashMap<u64, cursor::StreamStatement>,
    pub(crate) aborted: std::collections::HashSet<u64>,
    pub(crate) next_stream: u64,
    pub(crate) closed: bool,
    /// What a SQL text needs, decided once per text and schema.
    plans: HashMap<String, Arc<Plan>>,
    /// Reset statements of finished cursors, ready for the same SQL text.
    pool: HashMap<String, Vec<cursor::RawStatement>>,
    pooled: usize,
    /// Changes whenever the frontend connection is replaced. A cursor only
    /// returns its statement to the pool while the generation is unchanged.
    pub(crate) generation: u64,
    schema: (u64, u64),
}

/// How a SQL text runs. `direct` statements write or manage transactions and
/// go through `Session::execute`; the others are read-only queries.
pub(crate) struct Plan {
    pub direct: bool,
    pub parameters: usize,
    pub columns: Vec<String>,
    pub parsed: Option<Statement>,
}

const PLAN_LIMIT: usize = 512;
const POOL_LIMIT: usize = 64;

impl Session {
    pub fn open(path: &Path, cache_bytes: usize) -> Result<Self> {
        let store = Arc::new(Mutex::new(Store::open(path, cache_bytes)?));
        let connection = Self::frontend(&store)?;
        let schema = schema_signature(&*guard(&store)?);
        Ok(Self { connection, store, explicit: false, metadata_dirty: false, active_cursors:0,
            streams: HashMap::new(), aborted: Default::default(), next_stream: 0, closed:false,
            plans: HashMap::new(), pool: HashMap::new(), pooled: 0, generation: 0, schema })
    }

    /// Replace the frontend connection after a schema, metadata or rollback
    /// change. Pooled statements belong to the old connection.
    fn rebuild(&mut self) -> Result<()> {
        self.abort_streams();
        self.forget_statements();
        self.connection = Self::frontend(&self.store)?;
        self.schema = schema_signature(&*guard(&self.store)?);
        Ok(())
    }

    /// WeeWX rewrites a daily-summary metadata row with every archive record.
    /// Refill those rows without replacing the connection.
    fn reload_metadata(&mut self) -> Result<()> {
        let metadata = guard(&self.store)?.snapshot.manifest.metadata.clone();
        for table in &metadata {
            self.connection.execute(&format!("DELETE FROM {}", quote(&table.name)), [])?;
            if table.rows.is_empty() { continue; }
            let mut insert = self.connection.prepare_cached(&format!("INSERT INTO {} VALUES ({})", quote(&table.name), vec!["?"; table.rows.first().map_or(0, Vec::len)].join(",")))?;
            for row in &table.rows { insert.execute(params_from_iter(row.iter().map(Cell::value)))?; }
        }
        Ok(())
    }

    /// Finalize the statements of all open cursors. Their next fetch fails,
    /// as SQLite aborts pending reads on ROLLBACK.
    fn abort_streams(&mut self) {
        for (id, statement) in self.streams.drain() {
            unsafe { rusqlite::ffi::sqlite3_finalize(statement.0); }
            self.aborted.insert(id);
        }
        self.active_cursors = 0;
    }

    fn forget_statements(&mut self) {
        for (_, statements) in self.pool.drain() { for statement in statements { statement.finalize(); } }
        self.pooled = 0;
        self.plans.clear();
        self.generation += 1;
    }

    pub(crate) fn plan(&mut self, sql: &str) -> Result<Arc<Plan>> {
        if let Some(plan) = self.plans.get(sql) { return Ok(plan.clone()); }
        let direct = managed(sql)? || simple_write(sql);
        let plan = if direct { Plan { direct, parameters: 0, columns: vec![], parsed: None } } else {
            let prepared = self.connection.prepare_cached(sql)?;
            let readonly = prepared.readonly();
            let parameters = prepared.parameter_count();
            let columns = prepared.column_names().into_iter().map(str::to_owned).collect();
            drop(prepared);
            // Parsed once per SQL text; kept only for statements the native
            // path could answer.
            let parsed = if readonly {
                let statements = Parser::parse_sql(&SQLiteDialect {},sql).ok();
                if let Some(statements) = &statements { ensure!(statements.len()==1,"execute accepts one SQL statement"); }
                statements.and_then(|mut s| s.pop()).filter(fast::candidate)
            } else { None };
            Plan { direct: !readonly, parameters, columns, parsed }
        };
        if self.plans.len() >= PLAN_LIMIT { self.plans.clear(); }
        let plan = Arc::new(plan);
        self.plans.insert(sql.to_owned(), plan.clone());
        Ok(plan)
    }

    pub(crate) fn take_statement(&mut self, sql: &str) -> Option<cursor::RawStatement> {
        let statement = self.pool.get_mut(sql)?.pop()?;
        self.pooled -= 1;
        Some(statement)
    }

    pub(crate) fn return_statement(&mut self, sql: String, statement: cursor::RawStatement) {
        if self.pooled >= POOL_LIMIT { statement.finalize(); return; }
        self.pool.entry(sql).or_default().push(statement);
        self.pooled += 1;
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
        // Archive rows are read live from the store. Only schema and metadata
        // rows live in the frontend connection.
        if changed {
            let schema = schema_signature(&*guard(&self.store)?);
            if schema.0 != self.schema.0 { self.rebuild()?; }
            else if schema.1 != self.schema.1 { self.reload_metadata()?; self.schema = schema; }
        }
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
            self.begin_store()?;
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
            self.rebuild()?;
            return Ok(empty());
        }
        let ddl = parsed.as_ref().and_then(|s| s.first()).is_some_and(|s| matches!(s, Statement::CreateTable(_) | Statement::AlterTable {..} | Statement::Drop {..}));
        let readonly = if ddl { false } else { self.connection.prepare_cached(trimmed)?.readonly() };
        let automatic = !readonly && !self.explicit;
        // Rows and statements of an open cursor would not survive a schema change.
        if ddl { ensure!(self.active_cursors == 0,"close streaming read cursors before changing the schema"); }
        if automatic { self.begin_store()?; self.connection.execute_batch("BEGIN")?; }
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
                    self.rebuild()?;
                } else if self.explicit { guard(&self.store)?.restore_statement(point)?; }
                Err(error)
            }
        }
    }

    /// Writes may run while this connection still streams a read. WeeWX
    /// rebuilds daily summaries that way: it iterates archive records and
    /// writes each finished day on the same connection.
    fn begin_store(&mut self) -> Result<()> {
        let mut store = guard(&self.store)?;
        if self.active_cursors > 0 { store.begin_keeping_snapshot() } else { store.begin() }
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
            self.rebuild()?;
            return Err(error);
        }
        self.explicit = false; self.metadata_dirty = false;
        self.schema = schema_signature(&*guard(&self.store)?);
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
        self.forget_statements();
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
        self.rebuild()?;
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

/// Hashes of what the frontend connection copies from the store: first the
/// table definitions, then the metadata rows.
fn schema_signature(store: &Store) -> (u64, u64) {
    use std::hash::{Hash, Hasher};
    let mut structure = std::collections::hash_map::DefaultHasher::new();
    let mut rows = std::collections::hash_map::DefaultHasher::new();
    let manifest = &store.snapshot.manifest;
    manifest.id.hash(&mut structure);
    for table in &manifest.tables { table.name.hash(&mut structure); table.sql.hash(&mut structure); table.columns.len().hash(&mut structure); }
    for table in &manifest.metadata {
        table.name.hash(&mut structure); table.sql.hash(&mut structure);
        serde_json::to_vec(&table.rows).unwrap_or_default().hash(&mut rows);
    }
    (structure.finish(), rows.finish())
}

impl Drop for Session {
    fn drop(&mut self) { self.forget_statements(); }
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
