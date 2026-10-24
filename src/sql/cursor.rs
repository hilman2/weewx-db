//! Streaming statement handles kept alive by their owning SQL session.

use super::*;
use std::collections::VecDeque;
use std::ptr;
use rusqlite::ffi;

/// A prepared statement owned by one session's frontend connection.
pub(crate) struct RawStatement(*mut ffi::sqlite3_stmt);

// Statements are only used while holding the session mutex.
unsafe impl Send for RawStatement {}

impl RawStatement {
    pub(crate) fn finalize(self) { unsafe { ffi::sqlite3_finalize(self.0); } }
}

/// The statement of an open cursor, as registered with its session.
#[derive(Clone, Copy)]
pub(crate) struct StreamStatement(pub(crate) *mut ffi::sqlite3_stmt);

// Only used while holding the session mutex.
unsafe impl Send for StreamStatement {}

pub struct Cursor {
    session: Arc<Mutex<Session>>,
    statement: *mut ffi::sqlite3_stmt,
    /// SQL text and frontend generation of `statement`, for returning it to
    /// the session's pool once the result is exhausted or closed.
    origin: Option<(String, u64)>,
    /// Registration of `statement` in the session's open streams.
    stream: u64,
    rows: VecDeque<Vec<Cell>>,
    pub columns: Vec<String>,
    pub rowcount: i64,
    pub engine: String,
    pub in_transaction: bool,
}

// The statement never outlives its connection: the Arc owns that connection.
// Every SQLite operation, including finalization, holds the session mutex.
unsafe impl Send for Cursor {}

impl Cursor {
    pub fn open(session: Arc<Mutex<Session>>, sql: &str, parameters: &[Cell]) -> Result<Self> {
        let mut locked = session.lock().map_err(|_|anyhow::anyhow!("connection mutex poisoned"))?;
        locked.refresh()?;
        let plan = locked.plan(sql)?;
        if plan.direct {
            let result = locked.execute(sql,parameters)?;
            let in_transaction = locked.in_transaction();
            drop(locked);
            return Ok(Self { session,statement:ptr::null_mut(),origin:None,stream:0,rows:result.rows.into(),columns:result.columns,rowcount:result.changes as i64,engine:result.engine,in_transaction });
        }
        ensure!(plan.parameters == parameters.len(),"incorrect number of SQL bindings");
        let in_transaction = locked.in_transaction();
        let native = match &plan.parsed {
            Some(statement) => { let mut store = guard(&locked.store)?; fast::query(&mut store,statement,parameters)? }
            None => None,
        };
        if let Some(rows) = native {
            drop(locked);
            return Ok(Self { session,statement:ptr::null_mut(),origin:None,stream:0,rows:rows.into(),columns:plan.columns.clone(),rowcount:-1,engine:"rust-native".into(),in_transaction });
        }
        let statement = match locked.take_statement(sql) {
            Some(statement) => statement.0,
            None => {
                let text = CString::new(sql)?;
                let mut statement = ptr::null_mut();
                unsafe {
                    let connection = locked.connection.handle();
                    let status = ffi::sqlite3_prepare_v3(connection,text.as_ptr(),-1,ffi::SQLITE_PREPARE_PERSISTENT,&mut statement,ptr::null_mut());
                    if status != ffi::SQLITE_OK { return Err(last_error(connection)); }
                }
                ensure!(!statement.is_null(),"SQL statement is empty");
                statement
            }
        };
        // SQLite copies bound values using SQLITE_TRANSIENT. No Python or Rust
        // parameter buffer is retained beyond preparation.
        if let Err(error) = unsafe { bind(statement,parameters) } { unsafe { ffi::sqlite3_finalize(statement); } return Err(error); }
        locked.active_cursors += 1;
        locked.next_stream += 1;
        let stream = locked.next_stream;
        locked.streams.insert(stream, StreamStatement(statement));
        let origin = Some((sql.to_owned(), locked.generation));
        drop(locked);
        Ok(Self { session,statement,origin,stream,rows:VecDeque::new(),columns:plan.columns.clone(),rowcount:-1,engine:"sql-rust-storage".into(),in_transaction })
    }

    pub fn fetchmany(&mut self, count: usize) -> Result<Vec<Vec<Cell>>> {
        let shared = self.session.clone();
        let mut session = shared.lock().map_err(|_|anyhow::anyhow!("connection mutex poisoned"))?;
        ensure!(!session.closed,"connection is closed");
        if !self.statement.is_null() && session.aborted.remove(&self.stream) {
            self.statement = ptr::null_mut();
            self.rows.clear();
            bail!("query aborted by ROLLBACK or a schema change on this connection");
        }
        ensure!(count.saturating_mul(self.columns.len().max(1)).saturating_mul(std::mem::size_of::<Cell>()) <= guard(&session.store)?.workspace_bytes,"fetch workspace exceeded; increase workspace_mib or reduce fetch size");
        let mut rows = Vec::with_capacity(count.min(4096));
        while rows.len() < count {
            if let Some(row) = self.rows.pop_front() { rows.push(row); continue; }
            if self.statement.is_null() { break; }
            // Column bytes remain valid until the next sqlite3_step. Each Cell
            // owns its copy before that next step is made.
            unsafe {
                let status = ffi::sqlite3_step(self.statement);
                if status == ffi::SQLITE_DONE { self.finalize(&mut session); break; }
                if status != ffi::SQLITE_ROW {
                    let error = last_error(session.connection.handle());
                    self.finalize(&mut session);
                    return Err(error);
                }
                let mut row = Vec::with_capacity(self.columns.len());
                for i in 0..self.columns.len() as i32 {
                    let value = match ffi::sqlite3_column_type(self.statement,i) {
                        ffi::SQLITE_NULL => Cell::Null,
                        ffi::SQLITE_INTEGER => Cell::Integer(ffi::sqlite3_column_int64(self.statement,i)),
                        ffi::SQLITE_FLOAT => Cell::Real(ffi::sqlite3_column_double(self.statement,i).to_bits()),
                        ffi::SQLITE_TEXT => {
                            let length = ffi::sqlite3_column_bytes(self.statement,i) as usize;
                            let text = ffi::sqlite3_column_text(self.statement,i);
                            Cell::Text(std::str::from_utf8(std::slice::from_raw_parts(text,length))?.to_owned())
                        }
                        _ => {
                            let length = ffi::sqlite3_column_bytes(self.statement,i) as usize;
                            let bytes = if length == 0 { vec![] } else { std::slice::from_raw_parts(ffi::sqlite3_column_blob(self.statement,i).cast::<u8>(),length).to_vec() };
                            Cell::Blob(bytes)
                        }
                    };
                    row.push(value);
                }
                rows.push(row);
            }
        }
        Ok(rows)
    }

    fn finalize(&mut self, session: &mut Session) {
        if !self.statement.is_null() && session.aborted.remove(&self.stream) {
            // The session already finalized the statement and released the cursor.
            self.statement = ptr::null_mut();
            return;
        }
        if !self.statement.is_null() {
            session.streams.remove(&self.stream);
            match self.origin.take() {
                Some((sql, generation)) if generation == session.generation => unsafe {
                    ffi::sqlite3_reset(self.statement);
                    ffi::sqlite3_clear_bindings(self.statement);
                    session.return_statement(sql, RawStatement(self.statement));
                },
                _ => unsafe { ffi::sqlite3_finalize(self.statement); },
            }
            self.statement = ptr::null_mut();
            session.active_cursors -= 1;
        }
    }

    pub fn close(&mut self) -> Result<()> {
        let shared = self.session.clone();
        let mut session = shared.lock().map_err(|_|anyhow::anyhow!("connection mutex poisoned"))?;
        self.finalize(&mut session);
        self.rows.clear();
        Ok(())
    }
}

impl Drop for Cursor { fn drop(&mut self) { let _ = self.close(); } }

unsafe fn bind(statement: *mut ffi::sqlite3_stmt, parameters: &[Cell]) -> Result<()> {
    for (i,value) in parameters.iter().enumerate() {
        let index = i as i32+1;
        let status = unsafe { match value {
            Cell::Null => ffi::sqlite3_bind_null(statement,index),
            Cell::Integer(n) => ffi::sqlite3_bind_int64(statement,index,*n),
            Cell::Real(n) => ffi::sqlite3_bind_double(statement,index,f64::from_bits(*n)),
            Cell::Text(s) => ffi::sqlite3_bind_text64(statement,index,s.as_ptr().cast(),s.len() as u64,ffi::SQLITE_TRANSIENT(),ffi::SQLITE_UTF8 as u8),
            Cell::Blob(b) => ffi::sqlite3_bind_blob64(statement,index,b.as_ptr().cast(),b.len() as u64,ffi::SQLITE_TRANSIENT()),
        } };
        ensure!(status == ffi::SQLITE_OK,"parameter binding failed with SQLite status {status}");
    }
    Ok(())
}

unsafe fn last_error(connection: *mut ffi::sqlite3) -> anyhow::Error {
    anyhow::anyhow!("{}",unsafe { CStr::from_ptr(ffi::sqlite3_errmsg(connection)).to_string_lossy() })
}
