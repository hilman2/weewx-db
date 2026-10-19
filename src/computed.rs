//! Weather formulas evaluated on projected columns before aggregation.

use crate::*;
use crate::store::{RowCursor, Store};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Formula { Dewpoint, Humidex, Heatindex, HeatindexOld }

impl std::str::FromStr for Formula {
    type Err = anyhow::Error;
    fn from_str(name: &str) -> Result<Self> {
        match name {
            "dewpoint"=>Ok(Self::Dewpoint), "humidex"=>Ok(Self::Humidex),
            "heatindex"=>Ok(Self::Heatindex), "heatindex_old"=>Ok(Self::HeatindexOld),
            _=>bail!("unknown native weather formula: {name}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Computed {
    pub formula: Formula,
    pub temperature: String,
    pub humidity: String,
    pub unit_system: i64,
}

pub struct ComputedSummary { pub rows: u64, pub stats: Stats }

pub fn dewpoint_c(temperature: f64, humidity: f64) -> Option<f64> {
    if humidity <= 0.0 || temperature == -237.7 { return None; }
    let gamma = 17.27*temperature/(237.7+temperature)+(humidity/100.0).ln();
    let result = 237.7*gamma/(17.27-gamma);
    result.is_finite().then_some(result)
}

fn heatindex_f(t: f64, r: f64, old: bool) -> f64 {
    if old && (t < 80.0 || r < 40.0) || !old && t <= 40.0 { return t; }
    let mut hi = 0.5*(t+61.0+(t-68.0)*1.2+r*0.094);
    if old || (hi+t)/2.0 >= 80.0 {
        hi = -42.379+2.04901523*t+10.14333127*r-0.22475541*t*r
            -6.83783e-3*t.powi(2)-5.481717e-2*r.powi(2)
            +1.22874e-3*t.powi(2)*r+8.5282e-4*t*r.powi(2)
            -1.99e-6*t.powi(2)*r.powi(2);
        if old { return hi.max(t); }
        if r < 13.0 && t > 80.0 && t < 112.0 {
            hi -= ((13.0-r)/4.0)*((17.0-(t-95.0).abs())/17.0).sqrt();
        } else if r > 85.0 && (80.0..87.0).contains(&t) {
            hi += ((r-85.0)/10.0)*((87.0-t)/5.0);
        }
    }
    hi
}

impl Computed {
    pub fn value(&self, temperature: Option<f64>, humidity: Option<f64>) -> Option<f64> {
        let (t,r) = temperature.zip(humidity)?;
        let us = self.unit_system == 1;
        let celsius = if us { (t-32.0)/1.8 } else { t };
        let result = match self.formula {
            Formula::Dewpoint => {
                let c = dewpoint_c(celsius,r)?;
                if us { c*1.8+32.0 } else { c }
            }
            Formula::Humidex => {
                let dp_k = dewpoint_c(celsius,r)?+273.15;
                if dp_k == 0.0 { return None; }
                let e = 6.11*(5417.7530*(1.0/273.15-1.0/dp_k)).exp();
                let h = 0.5555*(e-10.0);
                let c = celsius+if h > 0.0 { h } else { 0.0 };
                if us { c*1.8+32.0 } else { c }
            }
            Formula::Heatindex|Formula::HeatindexOld => {
                let f = if us { t } else { t*1.8+32.0 };
                let result = heatindex_f(f,r,self.formula == Formula::HeatindexOld);
                if us { result } else { (result-32.0)/1.8 }
            }
        };
        result.is_finite().then_some(result)
    }
}

struct Projection { temperature: usize, humidity: usize, units: Option<usize> }

impl Store {
    fn projection(&self, table: &str, source: &Computed, check_units: bool) -> Result<Projection> {
        ensure!([1,16,17].contains(&source.unit_system),"unsupported unit system");
        let columns = self.columns(table)?;
        let find = |name:&str|columns.iter().position(|c|c.name.eq_ignore_ascii_case(name)).with_context(||format!("no such column: {name}"));
        Ok(Projection {temperature:find(&source.temperature)?,humidity:find(&source.humidity)?,
            units:if check_units { Some(find("usUnits")?) } else { None }})
    }

    fn scan_computed(&mut self, table: &str, source: &Computed, span: Span, check_units: bool,
        mut visit: impl FnMut(i64,Option<f64>)) -> Result<()> {
        ensure!(span.start <= span.stop,"reversed time span");
        let projection = self.projection(table,source,check_units)?;
        let ti = self.table_index(table)?;
        let definition = &self.snapshot.manifest.tables[ti];
        let ts = definition.columns.iter().position(|c|c.name == "dateTime").unwrap();
        let temperature_kind = definition.columns[projection.temperature].kind;
        let humidity_kind = definition.columns[projection.humidity].kind;
        let table = definition.name.clone();
        let blocks = &definition.blocks;
        let first = blocks.partition_point(|b|span.before(b.last));
        let stop = blocks.partition_point(|b|!span.after(b.first));
        let changes = self.changes.get(&table);
        for bi in first..stop {
            let n = self.snapshot.manifest.tables[ti].blocks[bi].rows;
            let times = self.snapshot.load(ti,bi,ts)?;
            let temperatures = self.snapshot.load(ti,bi,projection.temperature)?;
            let humidities = self.snapshot.load(ti,bi,projection.humidity)?;
            let units = projection.units.map(|ci|self.snapshot.load(ti,bi,ci)).transpose()?;
            let mut lower = 0; let mut upper = n;
            while lower < upper {
                let middle = lower+(upper-lower)/2;
                if span.before(timestamp(&times,n,middle)) { lower = middle+1; } else { upper = middle; }
            }
            for ri in lower..n {
                let time = timestamp(&times,n,ri);
                if span.after(time) { break; }
                if changes.is_some_and(|rows|rows.contains_key(&time)) { continue; }
                ensure!(bits(&times,n,ri).is_some(),"NULL timestamp");
                if let Some(units) = &units { ensure!(bits(units,n,ri) == Some(source.unit_system as u64),"unit system changes within computed series"); }
                let value = source.value(number(&temperatures,n,ri,temperature_kind),number(&humidities,n,ri,humidity_kind));
                visit(time,value);
            }
        }
        // Overlay rows replace base values before the nonlinear formula runs.
        // Aggregating input columns first would change the result.
        if let Some(changes) = changes {
            for (&time,row) in changes.iter() {
                if span.before(time) || span.after(time) { continue; }
                if let Some(row) = row {
                    if let Some(ci) = projection.units { ensure!(row[ci] == Cell::Integer(source.unit_system),"unit system changes within computed series"); }
                    visit(time,source.value(row[projection.temperature].numeric(),row[projection.humidity].numeric()));
                }
            }
        }
        Ok(())
    }

    pub fn computed_aggregate(&mut self, table: &str, source: &Computed, span: Span, check_units: bool) -> Result<Stats> {
        Ok(self.computed_summary(table,source,span,check_units)?.stats)
    }

    pub fn computed_summary(&mut self, table: &str, source: &Computed, span: Span, check_units: bool) -> Result<ComputedSummary> {
        let mut result = Accumulator::default();
        let mut rows = 0;
        self.scan_computed(table,source,span,check_units,|_,value| { rows += 1; result.add(value); })?;
        Ok(ComputedSummary {rows,stats:result.finish()})
    }

    pub fn computed_ranges(&mut self, table: &str, source: &Computed, ranges: &[(i64,i64)], check_units: bool) -> Result<Vec<Stats>> {
        Ok(self.computed_range_summaries(table,source,ranges,check_units)?.into_iter().map(|s|s.stats).collect())
    }

    fn computed_range_summaries(&mut self, table: &str, source: &Computed, ranges: &[(i64,i64)], check_units: bool) -> Result<Vec<ComputedSummary>> {
        ensure!(ranges.len() <= self.workspace_bytes/(std::mem::size_of::<Accumulator>()+std::mem::size_of::<Stats>()+32),"computed interval workspace exceeded");
        self.projection(table,source,check_units)?;
        if ranges.is_empty() { return Ok(vec![]); }
        for (i,(start,stop)) in ranges.iter().enumerate() {
            ensure!(start <= stop && (i == 0 || ranges[i-1].1 <= *start),"intervals must be ordered and disjoint");
        }
        let span = Span {start:ranges[0].0,stop:ranges.last().unwrap().1,daily:false};
        let mut bins = vec![Accumulator::default();ranges.len()];
        let mut rows = vec![0;ranges.len()];
        let mut index = 0;
        self.scan_computed(table,source,span,check_units,|time,value| {
            if index == ranges.len() || time <= ranges[index].0 { index = ranges.partition_point(|r|r.1 < time); }
            while index < ranges.len() && time > ranges[index].1 { index += 1; }
            if index < ranges.len() && time > ranges[index].0 { rows[index] += 1; bins[index].add(value); }
        })?;
        Ok(bins.into_iter().zip(rows).map(|(bin,rows)|ComputedSummary {rows,stats:bin.finish()}).collect())
    }

    pub fn computed_series(&mut self, table: &str, source: &Computed, ranges: &[(i64,i64)], aggregate: &str) -> Result<(Vec<Cell>,u64)> {
        ensure!(["sum","avg","min","max","count","cumulative"].contains(&aggregate),"unsupported computed aggregate");
        let bins = self.computed_range_summaries(table,source,ranges,true)?;
        let source_rows = bins.iter().map(|s|s.rows).sum();
        let mut cumulative = Accumulator::default();
        let values = bins.into_iter().map(|summary| {
            let stats = summary.stats;
            if aggregate == "count" { return Cell::Integer(stats.count as i64); }
            let result = match aggregate {
                "sum"=>Some(stats.sum.unwrap_or(0.0)),"avg"=>stats.sum.map(|sum|sum/stats.count as f64),
                "min"=>stats.min,"max"=>stats.max,
                _=>{ cumulative.merge(&stats); Some(cumulative.clone().finish().sum.unwrap_or(0.0)) },
            };
            result.map(|n|Cell::Real(n.to_bits())).unwrap_or(Cell::Null)
        }).collect();
        Ok((values,source_rows))
    }

    pub fn computed_raw(&mut self, table: &str, source: &Computed, start: i64, stop: i64) -> Result<(Vec<i64>,Vec<i64>,Vec<Cell>)> {
        let projection = self.projection(table,source,true)?;
        let interval = self.column_defs(table)?.iter().position(|c|c.name == "interval").context("interval column missing")?;
        let mut cursor = RowCursor::new(self,table,Span {start,stop,daily:false})?;
        let mut starts = Vec::new(); let mut stops = Vec::new(); let mut values = Vec::new();
        while cursor.advance(self)? {
            ensure!(values.len() < self.workspace_bytes/(std::mem::size_of::<Cell>()+16),"computed raw series workspace exceeded");
            ensure!(cursor.cell(self,projection.units.unwrap())? == Cell::Integer(source.unit_system),"unit system changes within computed series");
            let Cell::Integer(minutes) = cursor.cell(self,interval)? else { bail!("invalid archive interval") };
            let seconds = minutes.checked_mul(60).context("archive interval overflow")?;
            starts.push(cursor.time.checked_sub(seconds).context("archive timestamp overflow")?);
            stops.push(cursor.time);
            let value = source.value(cursor.cell(self,projection.temperature)?.numeric(),cursor.cell(self,projection.humidity)?.numeric());
            values.push(value.map(|n|Cell::Real(n.to_bits())).unwrap_or(Cell::Null));
        }
        Ok((starts,stops,values))
    }
}
