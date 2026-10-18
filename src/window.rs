//! Stateful weather calculations retain WeeWX's provider state across calls.

use crate::*;
use crate::analytics::Query;
use crate::store::{Store,RowCursor};
use crate::weather::{WeatherSpec,Parameters,fahrenheit};
use serde_json::{Value as Json,json};

#[derive(Default)]
struct RollingColumn {
    sum: Accumulator, count: u64,
    min: VecDeque<(i64,f64)>, max: VecDeque<(i64,f64)>,
}

impl RollingColumn {
    fn add(&mut self, time: i64, value: Option<f64>) {
        let Some(value) = value else {return};
        self.sum.add(Some(value)); self.count += 1;
        while self.min.back().is_some_and(|(_,v)|*v > value) {self.min.pop_back();}
        while self.max.back().is_some_and(|(_,v)|*v < value) {self.max.pop_back();}
        self.min.push_back((time,value)); self.max.push_back((time,value));
    }
    fn remove(&mut self, time: i64, value: Option<f64>) {
        let Some(value) = value else {return};
        self.sum.add(Some(-value)); self.count -= 1;
        if self.min.front().is_some_and(|(t,_)|*t == time) {self.min.pop_front();}
        if self.max.front().is_some_and(|(t,_)|*t == time) {self.max.pop_front();}
        if self.count == 0 {self.sum = Accumulator::default();}
    }
    fn mean(&self) -> Option<f64> { if self.count == 0 {None} else {self.sum.clone().finish().sum.map(|v|v/self.count as f64)} }
}

struct EtWindow {
    cursor: RowCursor, columns: [Option<usize>;5], next: Option<(i64,[Option<f64>;5])>,
    rows: VecDeque<(i64,[Option<f64>;5])>, stats: [RollingColumn;5], period: i64,
    available: bool,
}

impl EtWindow {
    fn new(store: &mut Store, query: &Query, p: &Parameters) -> Result<Self> {
        let start = query.ranges.iter().map(|r|r.0).min().unwrap();
        let stop = query.ranges.iter().map(|r|r.1).max().unwrap();
        let period = p.number("et_period",3600.0) as i64;
        ensure!(period > 0,"ET period must be positive");
        let names = ["outTemp","radiation","windSpeed","outHumidity","usUnits"];
        let schema = store.columns(&query.table)?;
        let columns = names.map(|name|schema.iter().position(|c|c.name == name));
        Ok(Self {cursor:RowCursor::new(store,&query.table,Span {start:start.saturating_sub(period),stop,daily:false})?,
            columns,next:None,rows:VecDeque::new(),stats:std::array::from_fn(|_|RollingColumn::default()),period,available:false})
    }
    fn advance(&mut self, store: &mut Store, time: i64) -> Result<()> {
        loop {
            if self.next.is_none() && self.cursor.advance(store)? {
                let mut values = [None;5];
                for (i,column) in self.columns.iter().enumerate() {values[i] = column.map(|ci|self.cursor.cell(store,ci)).transpose()?.and_then(|c|c.numeric());}
                self.next = Some((self.cursor.time,values));
            }
            let Some((stamp,values)) = self.next.filter(|(stamp,_)|*stamp <= time) else {break};
            self.next = None;
            if stamp <= time.saturating_sub(self.period) {continue;}
            ensure!(self.rows.len() < store.workspace_bytes/512,"ET rolling workspace exceeded");
            for (column,value) in self.stats.iter_mut().zip(values) {column.add(stamp,value);}
            self.rows.push_back((stamp,values));
        }
        while self.rows.front().is_some_and(|(stamp,_)|*stamp <= time.saturating_sub(self.period)) {
            let (stamp,values) = self.rows.pop_front().unwrap();
            for (column,value) in self.stats.iter_mut().zip(values) {column.remove(stamp,value);}
        }
        Ok(())
    }
    fn evaluate(&mut self, store: &mut Store, time: i64, interval: Option<f64>, p: &Parameters) -> Result<Option<f64>> {
        self.advance(store,time)?;
        self.available = false;
        let s = &self.stats;
        let Some(unit) = s[4].min.front().map(|(_,v)|*v as i64) else {return Ok(None)};
        if s[4].min.front() != s[4].max.front() && s[4].min.front().map(|(_,v)|v) != s[4].max.front().map(|(_,v)|v) {return Ok(None);}
        let inputs = [s[0].min.front().map(|(_,v)|*v),s[0].max.front().map(|(_,v)|*v),
            s[3].min.front().map(|(_,v)|*v),s[3].max.front().map(|(_,v)|*v),s[1].mean(),s[2].mean()];
        let Some(interval) = interval else {return Ok(None)};
        let [Some(a),Some(b),Some(c),Some(d),Some(e),Some(f)] = inputs else {return Ok(None)};
        self.available = true;
        let inputs=[a,b,c,d,e,f];
        Ok(crate::weather::evapotranspiration(&inputs,time,unit,p).map(|rate|rate*interval/60.0))
    }
}

pub struct WindowState {
    table: String, et: Option<EtWindow>, previous: Option<f64>, lookup_time: Option<i64>,
    temperature_f: Option<f64>, temperature_present: bool, rain_events: Vec<(f64,f64)>, rain_units: Option<i64>, augmented: bool,
    rain_suffix: Vec<f64>, rain_order: Vec<(f64,f64)>,
}

impl WindowState {
    pub fn new(store: &mut Store, query: &Query) -> Result<Self> {
        let weather = query.weather.as_ref();
        let p = weather.map(|w|w.parameters.clone()).unwrap_or_default();
        let events: Vec<(f64,f64)> = p.values.get("rain_events").map(|v|serde_json::from_value(v.clone())).transpose()?.unwrap_or_default();
        ensure!(events.len() <= store.workspace_bytes/64,"rain event workspace exceeded");
        Ok(Self {table:query.table.clone(),
            et:if weather.is_some_and(|w|w.formula == "ET") {Some(EtWindow::new(store,query,&p)?)} else {None},
            previous:p.values.get("previous").and_then(Json::as_f64), lookup_time:p.values.get("lookup_time").and_then(Json::as_i64),
            temperature_f:p.values.get("temperature_f").and_then(Json::as_f64),rain_events:events,
            temperature_present:p.values.get("temperature_present").and_then(Json::as_bool).unwrap_or(false),
            rain_units:p.values.get("rain_units").and_then(Json::as_i64),augmented:p.values.get("augmented").and_then(Json::as_bool).unwrap_or(false),
            rain_suffix:vec![],rain_order:vec![]})
    }

    pub fn evaluate(&mut self, store: &mut Store, weather: &WeatherSpec, time: i64, args: &[Option<f64>]) -> Result<Option<f64>> {
        match weather.formula.as_str() {
            "ET" => self.et.as_mut().context("ET window missing")?.evaluate(store,time,args.first().copied().flatten(),&weather.parameters),
            "pressure" => {
                let target = time.saturating_sub(12*3600);
                let tolerance = weather.parameters.number("max_delta_12h",1800.0) as i64;
                if self.lookup_time.is_none_or(|prior|prior.abs_diff(target) > tolerance as u64) || !self.temperature_present {
                    let mut cursor = RowCursor::new_bounds(store,&self.table,Some((target.saturating_sub(tolerance),true)),Some((target.saturating_add(tolerance),true)))?;
                    let columns = store.columns(&self.table)?;
                    let temperature = columns.iter().position(|c|c.name == "outTemp");
                    let units = columns.iter().position(|c|c.name == "usUnits");
                    let mut nearest = u64::MAX; self.temperature_f = None; self.temperature_present = false;
                    while cursor.advance(store)? {
                        let distance = cursor.time.abs_diff(target);
                        if distance >= nearest {continue;}
                        nearest = distance;
                        self.temperature_present = temperature.is_some();
                        let t = temperature.map(|ci|cursor.cell(store,ci)).transpose()?.and_then(|c|c.numeric());
                        let u = units.map(|ci|cursor.cell(store,ci)).transpose()?.and_then(|c|c.numeric());
                        self.temperature_f = t.zip(u).map(|(t,u)|fahrenheit(t,u as i64));
                    }
                    self.lookup_time = Some(target);
                }
                let mut values = args.to_vec(); values.push(self.temperature_f);
                Ok(weather.value(time,&values))
            }
            "delta" => {
                let current = args.first().copied().flatten();
                let value = weather.value(time,&[current,self.previous]); self.previous = current;
                Ok(value)
            }
            "rainRate" => self.rain_rate(store,time,&weather.parameters),
            "windDir"|"windGustDir" => {
                ensure!(weather.parameters.number("force_null",1.0) != 0.0 && args.first().copied().flatten().unwrap_or(0.0) == 0.0,"no calculate");
                Ok(None)
            }
            _ => Ok(weather.value(time,args)),
        }
    }

    fn rain_rate(&mut self, store: &mut Store, time: i64, parameters: &Parameters) -> Result<Option<f64>> {
        let time = parameters.number("event_time",time as f64);
        let period = parameters.number("rain_period",900.0);
        ensure!(period.is_finite()&&period > 0.0,"rain period must be positive");
        if !self.augmented {
            let stop = self.rain_events.iter().map(|e|e.0).min_by(f64::total_cmp).map_or(time,|first|first.min(time));
            let start = time-parameters.number("retain_period",930.0);
            let columns = store.columns(&self.table)?;
            let rain = columns.iter().position(|c|c.name == "rain"); let units = columns.iter().position(|c|c.name == "usUnits");
            if let (Some(rain),Some(units)) = (rain,units) {
                let mut cursor = RowCursor::new(store,&self.table,Span {start:start.floor() as i64,stop:stop.max(start).floor() as i64,daily:false})?;
                while cursor.advance(store)? {
                    if self.rain_events.iter().any(|e|e.0 == cursor.time as f64) {continue;}
                    let Some(value) = cursor.cell(store,rain)?.numeric().filter(|r|*r != 0.0) else {continue};
                    let unit = cursor.cell(store,units)?.numeric().context("rain units missing")? as i64;
                    let target = *self.rain_units.get_or_insert(unit);
                    let factor = |u|match u {1=>25.4,16=>10.0,_=>1.0};
                    self.rain_events.push((cursor.time as f64,value*factor(unit)/factor(target)));
                    self.rain_events.retain(|(t,_)|*t >= cursor.time as f64-period);
                    ensure!(self.rain_events.len() <= store.workspace_bytes/64,"rain event workspace exceeded");
                }
            }
            self.rain_events.sort_by(|a,b|a.0.total_cmp(&b.0)); self.augmented = true;
        }
        if self.rain_suffix.is_empty() {
            self.rain_order=self.rain_events.clone();
            self.rain_order.sort_by(|a,b|a.0.total_cmp(&b.0));
            self.rain_suffix.resize(self.rain_events.len()+1,0.0);let mut sum=Accumulator::default();
            for (i,(_,value)) in self.rain_order.iter().enumerate().rev() {
                sum.add(Some(*value));self.rain_suffix[i]=sum.clone().finish().sum.unwrap_or(0.0);
            }
        }
        let index = self.rain_order.partition_point(|(t,_)|*t <= time-period);
        Ok(Some(3600.0*self.rain_suffix[index]/period))
    }

    pub fn finish(self) -> Json {json!({"previous":self.previous,"lookup_time":self.lookup_time,"temperature_f":self.temperature_f,"temperature_present":self.temperature_present,
        "rain_events":self.rain_events,"rain_units":self.rain_units,"augmented":self.augmented,
        "et_available":self.et.map(|et|et.available)})}
}
