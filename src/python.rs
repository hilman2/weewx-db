use crate::*;
use crate::sql::{Session, cursor::Cursor};
use crate::store::Store;
use std::sync::Mutex;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyInt, PyFloat, PyString, PyList, PyTuple};
use pyo3::exceptions::PyValueError;
use crate::computed::Computed;

fn py_error(error: impl std::fmt::Display) -> PyErr { PyValueError::new_err(error.to_string()) }
type AggregateResult = (u64,Option<f64>,Option<f64>,Option<f64>);
type ComputationSpec = (String,(String,String),i64);
type ComputedResult = (u64,u64,Option<f64>,Option<f64>,Option<f64>);

#[pyclass(name="Connection",module="_weewxdb")]
struct PyConnection { session: Arc<Mutex<Session>> }

#[pymethods]
impl PyConnection {
    #[new]
    #[pyo3(signature=(path,cache_mib=32,timeout=5.0,workspace_mib=32,sync_mode="background"))]
    fn new(py: Python<'_>, path: String, cache_mib: usize, timeout: f64, workspace_mib: usize, sync_mode: &str) -> PyResult<Self> {
        let cache = cache_mib.checked_mul(1024*1024).ok_or_else(||py_error("cache size overflow"))?;
        let workspace = workspace_mib.checked_mul(1024*1024).ok_or_else(||py_error("workspace size overflow"))?;
        let session = py.detach(|| -> Result<_> {
            let mut session = Session::open(Path::new(&path),cache)?;
            session.set_workspace(workspace)?;
            session.set_sync_mode(sync_mode.parse()?)?;
            crate::sql::guard(&session.store)?.set_timeout(timeout)?;
            Ok(session)
        }).map_err(py_error)?;
        Ok(Self { session:Arc::new(Mutex::new(session)) })
    }

    #[getter]
    fn in_transaction(&self) -> PyResult<bool> {
        Ok(self.session.lock().map_err(|_|py_error("connection mutex poisoned"))?.in_transaction())
    }

    #[pyo3(signature=(sql,parameters=None))]
    fn execute(&self, py: Python<'_>, sql: String, parameters: Option<Vec<Py<PyAny>>>) -> PyResult<PyCursor> {
        let parameters = parameters.unwrap_or_default().iter().map(|v|from_python(v.bind(py))).collect::<PyResult<Vec<_>>>()?;
        let cursor = py.detach(||Cursor::open(self.session.clone(),&sql,&parameters)).map_err(py_error)?;
        Ok(PyCursor { cursor:Mutex::new(cursor) })
    }

    fn close(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(||self.session.lock().map_err(|_|py_error("connection mutex poisoned"))?.close().map_err(py_error))
    }

    fn flush(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(||self.session.lock().map_err(|_|py_error("connection mutex poisoned"))?.flush().map_err(py_error))
    }

    #[pyo3(signature=(rebuild=false))]
    fn checkpoint(&self, py: Python<'_>, rebuild: bool) -> PyResult<()> {
        py.detach(||self.session.lock().map_err(|_|py_error("connection mutex poisoned"))?.checkpoint(rebuild).map_err(py_error))
    }

    #[pyo3(signature=(table,column,start,stop,daily=false))]
    fn aggregate(&self, py: Python<'_>, table: String, column: String, start: i64, stop: i64, daily: bool) -> PyResult<AggregateResult> {
        let stats = py.detach(|| -> Result<Stats> {
            let mut session = self.session.lock().map_err(|_|anyhow::anyhow!("connection mutex poisoned"))?;
            session.refresh()?;
            crate::sql::guard(&session.store)?.aggregate(&table,&column,Span {start,stop,daily})
        }).map_err(py_error)?;
        Ok((stats.count,stats.sum,stats.min,stats.max))
    }

    fn series(&self, py: Python<'_>, table: String, column: String, ranges: Vec<(i64,i64)>, aggregate: String) -> PyResult<Py<PyAny>> {
        let values = py.detach(|| -> Result<Vec<Cell>> {
            let mut session = self.session.lock().map_err(|_|anyhow::anyhow!("connection mutex poisoned"))?;
            session.refresh()?;
            crate::sql::guard(&session.store)?.series_ranges(&table,&column,&ranges,&aggregate)
        }).map_err(py_error)?;
        let list = PyList::empty(py);
        for value in &values { list.append(to_python(py,value)?)?; }
        Ok(list.into_any().unbind())
    }

    fn raw_series(&self, py: Python<'_>, table: String, column: String, start: i64, stop: i64) -> PyResult<(Vec<i64>,Vec<i64>,Py<PyAny>)> {
        let (starts,stops,values) = py.detach(|| -> Result<_> {
            let mut session = self.session.lock().map_err(|_|anyhow::anyhow!("connection mutex poisoned"))?;
            session.refresh()?;
            crate::sql::guard(&session.store)?.raw_series(&table,&column,start,stop)
        }).map_err(py_error)?;
        let list = PyList::empty(py);
        for value in &values { list.append(to_python(py,value)?)?; }
        Ok((starts,stops,list.into_any().unbind()))
    }

    fn computed_aggregate(&self, py: Python<'_>, table: String, specification: ComputationSpec, start: i64, stop: i64) -> PyResult<ComputedResult> {
        let source = computed_source(specification)?;
        let summary = py.detach(|| -> Result<crate::computed::ComputedSummary> {
            let mut session = self.session.lock().map_err(|_|anyhow::anyhow!("connection mutex poisoned"))?;
            session.refresh()?;
            crate::sql::guard(&session.store)?.computed_summary(&table,&source,Span {start,stop,daily:false},true)
        }).map_err(py_error)?;
        Ok((summary.rows,summary.stats.count,summary.stats.sum,summary.stats.min,summary.stats.max))
    }

    fn computed_series(&self, py: Python<'_>, table: String, specification: ComputationSpec, ranges: Vec<(i64,i64)>, aggregate: String) -> PyResult<(Py<PyAny>,u64)> {
        let source = computed_source(specification)?;
        let (values,source_rows) = py.detach(|| -> Result<(Vec<Cell>,u64)> {
            let mut session = self.session.lock().map_err(|_|anyhow::anyhow!("connection mutex poisoned"))?;
            session.refresh()?;
            crate::sql::guard(&session.store)?.computed_series(&table,&source,&ranges,&aggregate)
        }).map_err(py_error)?;
        let list = PyList::empty(py);
        for value in &values { list.append(to_python(py,value)?)?; }
        Ok((list.into_any().unbind(),source_rows))
    }

    fn computed_raw(&self, py: Python<'_>, table: String, specification: ComputationSpec, start: i64, stop: i64) -> PyResult<(Vec<i64>,Vec<i64>,Py<PyAny>)> {
        let source = computed_source(specification)?;
        let (starts,stops,values) = py.detach(|| -> Result<_> {
            let mut session = self.session.lock().map_err(|_|anyhow::anyhow!("connection mutex poisoned"))?;
            session.refresh()?;
            crate::sql::guard(&session.store)?.computed_raw(&table,&source,start,stop)
        }).map_err(py_error)?;
        let list = PyList::empty(py);
        for value in &values { list.append(to_python(py,value)?)?; }
        Ok((starts,stops,list.into_any().unbind()))
    }

    fn analytics(&self, py: Python<'_>, specification: String) -> PyResult<String> {
        let query: crate::analytics::Query = serde_json::from_str(&specification).map_err(py_error)?;
        py.detach(|| -> Result<String> {
            let mut session = self.session.lock().map_err(|_|anyhow::anyhow!("connection mutex poisoned"))?;
            session.refresh()?;
            let answer = crate::sql::guard(&session.store)?.analytics(&query)?;
            Ok(serde_json::to_string(&answer)?)
        }).map_err(py_error)
    }
}

fn computed_source((formula,inputs,unit_system): ComputationSpec) -> PyResult<Computed> {
    Ok(Computed {formula:formula.parse().map_err(py_error)?,temperature:inputs.0,humidity:inputs.1,unit_system})
}

#[pyclass(name="Cursor",module="_weewxdb")]
struct PyCursor { cursor: Mutex<Cursor> }

#[pymethods]
impl PyCursor {
    #[getter]
    fn columns(&self) -> PyResult<Vec<String>> { Ok(self.cursor.lock().map_err(|_|py_error("cursor mutex poisoned"))?.columns.clone()) }
    #[getter]
    fn rowcount(&self) -> PyResult<i64> { Ok(self.cursor.lock().map_err(|_|py_error("cursor mutex poisoned"))?.rowcount) }
    #[getter]
    fn engine(&self) -> PyResult<String> { Ok(self.cursor.lock().map_err(|_|py_error("cursor mutex poisoned"))?.engine.clone()) }

    #[pyo3(signature=(size=200))]
    fn fetchmany(&self, py: Python<'_>, size: usize) -> PyResult<Py<PyAny>> {
        let rows = py.detach(||self.cursor.lock().map_err(|_|py_error("cursor mutex poisoned"))?.fetchmany(size).map_err(py_error))?;
        let list = PyList::empty(py);
        for row in rows {
            let cells = row.iter().map(|v|to_python(py,v)).collect::<PyResult<Vec<_>>>()?;
            list.append(PyTuple::new(py,cells)?)?;
        }
        Ok(list.into_any().unbind())
    }

    fn fetchone(&self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
        let rows = py.detach(||self.cursor.lock().map_err(|_|py_error("cursor mutex poisoned"))?.fetchmany(1).map_err(py_error))?;
        rows.first().map(|row| {
            let cells = row.iter().map(|v|to_python(py,v)).collect::<PyResult<Vec<_>>>()?;
            Ok(PyTuple::new(py,cells)?.into_any().unbind())
        }).transpose()
    }

    fn close(&self, py: Python<'_>) -> PyResult<()> { py.detach(||self.cursor.lock().map_err(|_|py_error("cursor mutex poisoned"))?.close().map_err(py_error)) }
}

fn from_python(value: &Bound<'_,PyAny>) -> PyResult<Cell> {
    Ok(if value.is_none() { Cell::Null }
        else if value.is_instance_of::<PyInt>() { Cell::Integer(value.extract()?) }
        else if value.is_instance_of::<PyFloat>() { Cell::Real(value.extract::<f64>()?.to_bits()) }
        else if value.is_instance_of::<PyString>() { Cell::Text(value.extract()?) }
        else if value.is_instance_of::<PyBytes>() { Cell::Blob(value.extract()?) }
        else { return Err(py_error("SQL parameters must be int, float, str, bytes or None")); })
}

fn to_python(py: Python<'_>, value: &Cell) -> PyResult<Py<PyAny>> {
    Ok(match value {
        Cell::Null => py.None(),
        Cell::Integer(n) => n.into_pyobject(py)?.into_any().unbind(),
        Cell::Real(n) => f64::from_bits(*n).into_pyobject(py)?.into_any().unbind(),
        Cell::Text(s) => s.into_pyobject(py)?.into_any().unbind(),
        Cell::Blob(b) => PyBytes::new(py,b).into_any().unbind(),
    })
}

#[pyfunction]
fn create(py: Python<'_>, path: String) -> PyResult<()> { py.detach(||Store::create(Path::new(&path),"archive")).map_err(py_error) }

#[pyfunction]
fn drop_database(py: Python<'_>, path: String) -> PyResult<()> { py.detach(||Store::drop_database(Path::new(&path))).map_err(py_error) }

#[pyfunction]
fn astronomy(py:Python<'_>,specification:String)->PyResult<String>{
    let request:crate::astro::Request=serde_json::from_str(&specification).map_err(py_error)?;
    py.detach(||->Result<String>{Ok(serde_json::to_string(&crate::astro::execute(&request)?)?)}).map_err(py_error)
}

#[pymodule]
fn _weewxdb(module: &Bound<'_,PyModule>) -> PyResult<()> {
    module.add_class::<PyConnection>()?;
    module.add_class::<PyCursor>()?;
    module.add_function(wrap_pyfunction!(create,module)?)?;
    module.add_function(wrap_pyfunction!(drop_database,module)?)?;
    module.add_function(wrap_pyfunction!(astronomy,module)?)?;
    Ok(())
}
