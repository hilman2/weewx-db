use crate::*;
use crate::sql::{Session, cursor::Cursor};
use crate::store::Store;
use std::sync::Mutex;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyInt, PyFloat, PyString, PyList, PyTuple, PyDict, PyBool};
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

    fn packet_native(&self, py: Python<'_>, table: String, record: &Bound<'_,PyAny>, specifications: &Bound<'_,PyAny>) -> PyResult<Py<PyAny>> {
        let mut record = python_json(record,0)?;
        let specifications: Vec<(String,crate::weather::WeatherSpec)> = serde_json::from_value(python_json(specifications,0)?).map_err(py_error)?;
        let result = py.detach(|| -> Result<_> {
            let mut session = self.session.lock().map_err(|_|anyhow::anyhow!("connection mutex poisoned"))?;
            session.refresh()?;
            let mut store = crate::sql::guard(&session.store)?;
            let units = record["usUnits"].as_i64().context("packet unit system missing")?;
            let time = record["dateTime"].as_f64().context("packet timestamp missing")?;
            let mut results = Vec::new();
            for (observation, weather) in specifications {
                if weather.inputs.iter().any(|name|record.get(name).is_none()) {
                    let status = if matches!(observation.as_str(),"windDir"|"windGustDir") {"no_calculate"} else {"cannot_calculate"};
                    if status == "cannot_calculate" { record[&observation] = serde_json::Value::Null; }
                    results.push(serde_json::json!({"observation":observation,"status":status}));
                    continue;
                }
                let args: Vec<_> = weather.inputs.iter().map(|name|record[name].as_f64()).collect();
                let query: crate::analytics::Query = serde_json::from_value(serde_json::json!({
                    "table":table,"mode":"weather","observation":observation,"aggregate":"avg",
                    "ranges":[[time.floor() as i64,time.floor() as i64]],"weather":weather,"scalar_args":args
                }))?;
                let answer = match store.analytics(&query) {
                    Ok(answer) => answer,
                    Err(error) if error.to_string() == "no calculate" => {
                        results.push(serde_json::json!({"observation":observation,"status":"no_calculate"}));
                        continue;
                    },
                    Err(error) => return Err(error),
                };
                let mut value = answer.values[0].as_f64();
                let rain_scale = |u|if u == 1 {1.0} else if u == 17 {25.4} else {2.54};
                if observation == "ET" { value = value.map(|v|v*rain_scale(units)); }
                if observation == "pressure" && units != 1 { value = value.map(|v|v/0.0295299875); }
                if observation == "rainRate" {
                    let source_units = answer.state["rain_units"].as_i64().unwrap_or(units);
                    value = value.map(|v|v*rain_scale(units)/rain_scale(source_units));
                }
                record[&observation] = serde_json::json!(value);
                results.push(serde_json::json!({"observation":observation,"status":"calculated","value":value,"state":answer.state}));
            }
            Ok(serde_json::Value::Array(results))
        }).map_err(py_error)?;
        json_python(py,&result)
    }

    fn analytics_native(&self, py: Python<'_>, specification: &Bound<'_,PyAny>) -> PyResult<Py<PyAny>> {
        let query = serde_json::from_value(python_json(specification,0)?).map_err(py_error)?;
        let answer = py.detach(|| -> Result<_> {
            let mut session = self.session.lock().map_err(|_|anyhow::anyhow!("connection mutex poisoned"))?;
            session.refresh()?;
            crate::sql::guard(&session.store)?.analytics(&query)
        }).map_err(py_error)?;
        answer_python(py,&answer)
    }

    fn analytics_many_native(&self, py: Python<'_>, specifications: &Bound<'_,PyAny>) -> PyResult<Py<PyAny>> {
        let queries: Vec<crate::analytics::Query> = serde_json::from_value(python_json(specifications,0)?).map_err(py_error)?;
        let answer = py.detach(|| -> Result<_> {
            let mut session = self.session.lock().map_err(|_|anyhow::anyhow!("connection mutex poisoned"))?;
            session.refresh()?;
            crate::sql::guard(&session.store)?.analytics_many(&queries)
        }).map_err(py_error)?;
        let dict = PyDict::new(py);
        let answers = PyList::new(py,answer.answers.iter().map(|a|answer_python(py,a)).collect::<PyResult<Vec<_>>>()?)?;
        dict.set_item("answers",answers)?;
        dict.set_item("scans",answer.scans)?;
        dict.set_item("formula_evaluations",answer.formula_evaluations)?;
        Ok(dict.into_any().unbind())
    }

    fn revision(&self, py: Python<'_>) -> PyResult<Option<(String,u64)>> {
        py.detach(|| -> Result<_> {
            let mut session = self.session.lock().map_err(|_|anyhow::anyhow!("connection mutex poisoned"))?;
            session.refresh()?;
            if session.in_transaction() { return Ok(None); }
            let store = crate::sql::guard(&session.store)?;
            Ok(Some((store.snapshot.manifest.id.clone(),store.lsn())))
        }).map_err(py_error)
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

fn answer_python(py: Python<'_>, answer: &crate::analytics::Answer) -> PyResult<Py<PyAny>> {
    let dict = PyDict::new(py);
    let values = PyList::new(py,answer.values.iter().map(|v|json_python(py,v)).collect::<PyResult<Vec<_>>>()?)?;
    dict.set_item("values",values)?;
    dict.set_item("rows",&answer.rows)?;
    dict.set_item("units",&answer.units)?;
    dict.set_item("starts",&answer.starts)?;
    dict.set_item("stops",&answer.stops)?;
    dict.set_item("state",json_python(py,&answer.state)?)?;
    Ok(dict.into_any().unbind())
}

fn python_json(value: &Bound<'_,PyAny>, depth: usize) -> PyResult<serde_json::Value> {
    use serde_json::Value as J;
    if depth > 32 { return Err(py_error("native request nesting exceeded")); }
    Ok(if value.is_none() { J::Null }
        else if value.is_instance_of::<PyBool>() { J::Bool(value.extract()?) }
        else if value.is_instance_of::<PyInt>() { J::from(value.extract::<i64>()?) }
        else if value.is_instance_of::<PyFloat>() {
            J::Number(serde_json::Number::from_f64(value.extract()?).ok_or_else(||py_error("non-finite request number"))?)
        } else if value.is_instance_of::<PyString>() { J::String(value.extract()?) }
        else if let Ok(dict) = value.cast::<PyDict>() {
            let mut result = serde_json::Map::new();
            for (key,item) in dict.iter() { result.insert(key.extract()?,python_json(&item,depth+1)?); }
            J::Object(result)
        } else if value.is_instance_of::<PyList>() || value.is_instance_of::<PyTuple>() {
            J::Array(value.try_iter()?.map(|v|python_json(&v?,depth+1)).collect::<PyResult<_>>()?)
        } else { return Err(py_error("unsupported native request value")); })
}

fn json_python(py: Python<'_>, value: &serde_json::Value) -> PyResult<Py<PyAny>> {
    use serde_json::Value as J;
    Ok(match value {
        J::Null => py.None(),
        J::Bool(v) => v.into_pyobject(py)?.to_owned().into_any().unbind(),
        J::Number(v) => {
            if let Some(n) = v.as_i64() {n.into_pyobject(py)?.into_any().unbind()}
            else if let Some(n) = v.as_u64() {n.into_pyobject(py)?.into_any().unbind()}
            else {v.as_f64().unwrap().into_pyobject(py)?.into_any().unbind()}
        },
        J::String(v) => v.into_pyobject(py)?.into_any().unbind(),
        J::Array(values) => {
            PyList::new(py,values.iter().map(|v|json_python(py,v)).collect::<PyResult<Vec<_>>>()?)?.into_any().unbind()
        },
        J::Object(values) => {
            let dict = PyDict::new(py);
            for (k,v) in values { dict.set_item(k,json_python(py,v)?)?; }
            dict.into_any().unbind()
        },
    })
}

#[pyfunction]
fn astronomy_native(py:Python<'_>,specification:&Bound<'_,PyAny>)->PyResult<Py<PyAny>> {
    let request = serde_json::from_value(python_json(specification,0)?).map_err(py_error)?;
    let result = py.detach(||crate::astro::execute(&request)).map_err(py_error)?;
    json_python(py,&result)
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
    module.add_function(wrap_pyfunction!(astronomy_native,module)?)?;
    Ok(())
}
