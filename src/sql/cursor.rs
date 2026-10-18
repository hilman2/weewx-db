//! Streaming statement handles kept alive by their owning SQL session.

use super::*;
use std::collections::VecDeque;
use std::ptr;
use rusqlite::ffi;

pub struct Cursor {
    session: Arc<Mutex<Session>>,
    statement: *mut ffi::sqlite3_stmt,
    rows: VecDeque<Vec<Cell>>,
    pub columns: Vec<String>,
    pub rowcount: i64,
    pub engine: String,
}

// The statement never outlives its connection: the Arc owns that connection.
// Every SQLite operation, including finalization, holds the session mutex.
unsafe impl Send for Cursor {}

impl Cursor {
    pub fn open(session: Arc<Mutex<Session>>, sql: &str, parameters: &[Cell]) -> Result<Self> {
        let mut locked = session.lock().map_err(|_|anyhow::anyhow!("connection mutex poisoned"))?;
        locked.refresh()?;
        if managed(sql)? || simple_write(sql) {
            let result = locked.execute(sql,parameters)?;
            drop(locked);
            return Ok(Self { session,statement:ptr::null_mut(),rows:result.rows.into(),columns:result.columns,rowcount:result.changes as i64,engine:result.engine });
        }
        let prepared = locked.connection.prepare_cached(sql)?;
        ensure!(prepared.parameter_count() == parameters.len(),"incorrect number of SQL bindings");
        let readonly = prepared.readonly();
        drop(prepared);
        if !readonly {
            let result = locked.execute(sql,parameters)?;
            drop(locked);
            return Ok(Self { session,statement:ptr::null_mut(),rows:result.rows.into(),columns:result.columns,rowcount:result.changes as i64,engine:result.engine });
        }
        let parsed = if sql.contains('(') { Parser::parse_sql(&SQLiteDialect {},sql).ok() } else { None };
        if let Some(statements) = &parsed { ensure!(statements.len()==1,"execute accepts one SQL statement"); }
        if let Some(statement) = parsed.as_ref().and_then(|s|s.first()) {
            let native = fast::query(&mut *guard(&locked.store)?,statement,parameters)?;
            if let Some(rows) = native {
                let columns = locked.connection.prepare(sql)?.column_names().into_iter().map(str::to_owned).collect();
                drop(locked);
                return Ok(Self { session,statement:ptr::null_mut(),rows:rows.into(),columns,rowcount:-1,engine:"rust-native".into() });
            }
        }
        let sql = CString::new(sql)?;
        let mut statement = ptr::null_mut();
        // SQLite copies bound values using SQLITE_TRANSIENT. No Python or Rust
        // parameter buffer is retained beyond preparation.
        unsafe {
            let connection = locked.connection.handle();
            let status = ffi::sqlite3_prepare_v3(connection,sql.as_ptr(),-1,0,&mut statement,ptr::null_mut());
            if status != ffi::SQLITE_OK { return Err(last_error(connection)); }
            ensure!(!statement.is_null(),"SQL statement is empty");
            let binding = bind(statement,parameters);
            if let Err(error) = binding { ffi::sqlite3_finalize(statement); return Err(error); }
        }
        let columns = unsafe { (0..ffi::sqlite3_column_count(statement)).map(|i| CStr::from_ptr(ffi::sqlite3_column_name(statement,i)).to_string_lossy().into_owned()).collect() };
        locked.active_cursors += 1;
        drop(locked);
        Ok(Self { session,statement,rows:VecDeque::new(),columns,rowcount:-1,engine:"sql-rust-storage".into() })
    }

    pub fn fetchmany(&mut self, count: usize) -> Result<Vec<Vec<Cell>>> {
        let shared = self.session.clone();
        let mut session = shared.lock().map_err(|_|anyhow::anyhow!("connection mutex poisoned"))?;
        ensure!(!session.closed,"connection is closed");
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
        if !self.statement.is_null() {
            unsafe { ffi::sqlite3_finalize(self.statement); }
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
