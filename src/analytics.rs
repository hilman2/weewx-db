//! Streaming XType reductions over stored, calculated and daily observations.
//! Historical and threshold xaggs semantics follow Tom Keffer's weewx-xaggs
//! (copyright 2023-2024), pinned in reference.md. See vendor/LICENSE.xaggs.

use crate::*;
use crate::store::{Store,RowCursor};
use crate::weather::{WeatherSpec,finite};
use serde_json::{Value as Json,json};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Query {
    pub table: String,
    pub mode: String,
    pub observation: String,
    pub aggregate: String,
    pub ranges: Vec<(i64,i64)>,
    #[serde(default)] pub weather: Option<WeatherSpec>,
    #[serde(default)] pub unit_system: Option<i64>,
    #[serde(default)] pub threshold: Option<f64>,
    #[serde(default)] pub calendar_days: Vec<(i32,i32)>,
    #[serde(default)] pub raw: bool,
    #[serde(default)] pub lower_inclusive: bool,
    #[serde(default)] pub parameters: crate::weather::Parameters,
    #[serde(default)] pub scalar_args: Option<Vec<Option<f64>>>,
}

#[derive(Default, Serialize)]
pub struct Answer {
    pub values: Vec<Json>,
    pub rows: Vec<u64>,
    pub units: Vec<Option<i64>>,
    pub starts: Vec<i64>,
    pub stops: Vec<i64>,
    pub state: Json,
}

#[derive(Clone, Default)]
struct Reduction {
    rows: u64, values: Accumulator, first: Option<(i64,f64)>, last: Option<(i64,f64)>,
    min: Option<(i64,f64)>, max: Option<(i64,f64)>,
    mean: f64, m2: f64, squares: Accumulator,
    xy: f64, xx: f64, time_mean: f64, origin: Option<i64>,
    integral: Accumulator, seconds: Accumulator,
    samples: Vec<f64>, current_streak: u64, longest_streak: u64,
    longest_gap: i64, previous_time: Option<i64>,
}

impl Reduction {
    fn add(&mut self, time: i64, value: Option<f64>, interval: Option<f64>, aggregate: &str, threshold: Option<f64>, samples_left: &mut usize) -> Result<()> {
        self.rows += 1;
        if let Some(last) = self.previous_time { self.longest_gap = self.longest_gap.max(time.saturating_sub(last)); }
        self.previous_time = Some(time);
        if aggregate == "longest_ge" || aggregate == "longest_lt" {
            let limit = threshold.context("streak threshold missing")?;
            let yes = value.is_some_and(|v|if aggregate == "longest_ge" {v >= limit} else {v < limit});
            self.current_streak = if yes {self.current_streak+1} else {0};
            self.longest_streak = self.longest_streak.max(self.current_streak);
        }
        let Some(value) = value else { return Ok(()) };
        let prior = self.values.stats.count;
        self.values.add(Some(value));
        if self.first.is_none() { self.first = Some((time,value)); }
        self.last = Some((time,value));
        if self.min.is_none_or(|(_,v)|value < v) { self.min = Some((time,value)); }
        if self.max.is_none_or(|(_,v)|value > v) { self.max = Some((time,value)); }
        if matches!(aggregate,"stddev"|"stddev_samp"|"variance"|"rms"|"slope") {
            let count = prior+1;
            let delta = value-self.mean;
            self.mean += delta/count as f64;
            self.m2 += delta*(value-self.mean);
            if aggregate == "rms" { self.squares.add(Some(value*value)); }
            if aggregate == "slope" {
                let origin = *self.origin.get_or_insert(time);
                let x = (time as i128-origin as i128) as f64;
                let dx = x-self.time_mean;
                self.time_mean += dx/count as f64;
                self.xx += dx*(x-self.time_mean);
                self.xy += dx*(value-self.mean);
            }
        }
        if matches!(aggregate,"median"|"percentile") {
            ensure!(*samples_left > 0,"exact percentile workspace exceeded");
            *samples_left -= 1;
            self.samples.push(value);
        }
        if let Some(seconds) = interval {
            self.integral.add(Some(value*seconds)); self.seconds.add(Some(seconds));
        }
        Ok(())
    }

    fn finish(&self, aggregate: &str, null_sum_zero: bool, threshold: Option<f64>) -> Result<Json> {
        let stats = self.values.clone().finish();
        let value = match aggregate {
            "count" => return Ok(json!(stats.count)),
            "not_null" => return Ok(json!(stats.count > 0)),
            "longest_ge"|"longest_lt" => return Ok(json!(self.longest_streak)),
            "maxgap" => return Ok(if self.rows > 1 {json!(self.longest_gap)} else {Json::Null}),
            "sum"|"cumulative" => if null_sum_zero {Some(stats.sum.unwrap_or(0.0))} else {stats.sum},
            "avg" => stats.sum.map(|v|v/stats.count as f64),
            "min" => stats.min, "max" => stats.max,
            "mintime" => return Ok(self.min.map(|(t,_)|json!(t)).unwrap_or(Json::Null)),
            "maxtime" => return Ok(self.max.map(|(t,_)|json!(t)).unwrap_or(Json::Null)),
            "first" => self.first.map(|(_,v)|v), "last" => self.last.map(|(_,v)|v),
            "firsttime" => return Ok(self.first.map(|(t,_)|json!(t)).unwrap_or(Json::Null)),
            "lasttime" => return Ok(self.last.map(|(t,_)|json!(t)).unwrap_or(Json::Null)),
            "diff" => self.first.zip(self.last).map(|((_,a),(_,b))|b-a),
            "tderiv" => self.first.zip(self.last).and_then(|((a,x),(b,y))|if b != a {Some((y-x)/(b as i128-a as i128) as f64)} else {None}),
            "stddev" => (stats.count > 0).then(||(self.m2/stats.count as f64).max(0.0).sqrt()),
            "stddev_samp" => (stats.count > 1).then(||(self.m2/(stats.count-1) as f64).max(0.0).sqrt()),
            "variance" => (stats.count > 0).then(||(self.m2/stats.count as f64).max(0.0)),
            "rms" => self.squares.clone().finish().sum.map(|v|(v/stats.count as f64).sqrt()),
            "slope" => (self.xx > 0.0).then(||self.xy/self.xx),
            "integral" => self.integral.clone().finish().sum,
            "weighted_avg" => self.integral.clone().finish().sum.zip(self.seconds.clone().finish().sum).and_then(|(s,w)|(w != 0.0).then_some(s/w)),
            "median"|"percentile" => {
                let fraction = if aggregate == "median" {0.5} else {threshold.context("percentile fraction missing")?};
                ensure!((0.0..=1.0).contains(&fraction),"percentile fraction must be between 0 and 1");
                if self.samples.is_empty() {None} else {
                    let mut samples = self.samples.clone(); samples.sort_unstable_by(f64::total_cmp);
                    let pos = fraction*(samples.len()-1) as f64;
                    let a = samples[pos.floor() as usize]; let b = samples[pos.ceil() as usize];
                    Some(a+(b-a)*pos.fract())
                }
            }
            _ => bail!("unknown native aggregate: {aggregate}"),
        };
        Ok(value.and_then(finite).map_or(Json::Null,|v|json!(v)))
    }
}

fn indices(store: &Store, table: &str, names: &[String]) -> Result<Vec<Option<usize>>> {
    let columns = store.columns(table)?;
    Ok(names.iter().map(|name|columns.iter().position(|c|c.name.eq_ignore_ascii_case(name))).collect())
}

fn numbers(store: &mut Store, cursor: &mut RowCursor, columns: &[Option<usize>], values: &mut [Option<f64>]) -> Result<()> {
    for (value,column) in values.iter_mut().zip(columns) { *value = column.map(|ci|cursor.cell(store,ci)).transpose()?.and_then(|c|c.numeric()); }
    Ok(())
}

fn destination(ranges: &[(i64,i64)], time: i64, daily: bool) -> Option<usize> {
    let index = ranges.partition_point(|(_,stop)|if daily {*stop <= time} else {*stop < time});
    ranges.get(index).filter(|(start,_)|if daily {time >= *start} else {time > *start}).map(|_|index)
}

impl Store {
    pub fn analytics(&mut self, query: &Query) -> Result<Answer> {
        ensure!(query.ranges.len() <= self.workspace_bytes/(std::mem::size_of::<DayBin>()+128),"analytics interval workspace exceeded");
        for (start,stop) in &query.ranges { ensure!(start <= stop,"reversed analytics range"); }
        if !matches!(query.mode.as_str(),"degree"|"historical") {
            ensure!(query.ranges.windows(2).all(|r|r[0].1 <= r[1].0),"analytics ranges must be ordered and disjoint");
        }
        if query.ranges.is_empty() { return Ok(Answer::default()); }
        if query.mode=="archive"&&query.weather.is_none()&&!query.raw&&query.ranges.len()==1
            &&matches!(query.aggregate.as_str(),"sum"|"avg"|"count"|"min"|"max"|"not_null") {
            let (start,stop)=query.ranges[0];
            let stats=self.aggregate(&query.table,&query.observation,Span{start,stop,daily:false})?;
            let integer=self.columns(&query.table)?.iter().find(|c|c.name==query.observation).is_some_and(|c|c.kind==Kind::Integer);
            ensure!(!integer||matches!(query.aggregate.as_str(),"count"|"not_null")||![stats.min,stats.max,stats.sum].into_iter().flatten().any(|n|n.abs()>=(1_u64<<53)as f64),"integer precision requires SQL");
            let value=match query.aggregate.as_str(){
                "count"=>json!(stats.count),"not_null"=>json!(stats.count>0),
                "avg"=>stats.sum.map_or(Json::Null,|v|json!(v/stats.count as f64)),
                name=>{let value=match name{"sum"=>stats.sum,"min"=>stats.min,_=>stats.max};value.map_or(Json::Null,|v|if integer{json!(v as i64)}else{json!(v)})},
            };
            return Ok(Answer{values:vec![value],rows:vec![stats.count],units:vec![query.unit_system],..Answer::default()});
        }
        if query.mode == "daily" || query.mode == "historical" || query.mode == "degree" || query.mode == "xagg_avg" {
            return self.daily_analytics(query);
        }
        if query.mode == "vector" || query.mode == "wind" { return self.vector_analytics(query); }
        if query.mode == "gustdir" || query.mode == "archive" && matches!(query.aggregate.as_str(),"diff"|"tderiv") {return self.archive_special(query);}
        self.weather_analytics(query)
    }

    fn weather_analytics(&mut self, query: &Query) -> Result<Answer> {
        if let Some(args) = &query.scalar_args {
            let mut state = crate::window::WindowState::new(self,query)?;
            let value = state.evaluate(self,query.weather.as_ref().context("scalar formula missing")?,query.ranges[0].1,args)?;
            return Ok(Answer {values:vec![value.map_or(Json::Null,|v|json!(v))],state:state.finish(),..Answer::default()});
        }
        let mut names = query.weather.as_ref().map(|w|w.inputs.clone()).unwrap_or_else(||vec![query.observation.clone()]);
        let inputs = names.len(); names.extend(["usUnits".into(),"interval".into()]);
        let columns = indices(self,&query.table,&names)?;
        if query.weather.is_none() { ensure!(columns[0].is_some(),"unknown observation"); }
        if query.weather.is_none()&&self.columns(&query.table)?[columns[0].unwrap()].kind==Kind::Integer {
            let stats=self.aggregate(&query.table,&query.observation,Span{start:query.ranges[0].0,stop:query.ranges.last().unwrap().1,daily:false})?;
            ensure!(![stats.min,stats.max,stats.sum].into_iter().flatten().any(|n|n.abs()>=(1_u64<<53)as f64),"integer precision requires SQL");
        }
        let start = query.ranges.iter().map(|r|r.0).min().unwrap();
        let stop = query.ranges.iter().map(|r|r.1).max().unwrap();
        let mut cursor = RowCursor::new_bounds(self,&query.table,Some((start,query.lower_inclusive)),Some((stop,true)))?;
        let mut reductions = vec![Reduction::default();query.ranges.len()];
        // Account for every bin together, including Vec growth and the copy
        // used to sort exact percentiles. A limit per bin can exceed the budget.
        let mut samples_left = self.workspace_bytes.saturating_sub(reductions.len()*(std::mem::size_of::<Reduction>()+128))/32;
        let mut units = vec![None;query.ranges.len()];
        let mut values = vec![None;names.len()]; let mut answer = Answer::default();
        let mut state = crate::window::WindowState::new(self,query)?;
        while cursor.advance(self)? {
            numbers(self,&mut cursor,&columns,&mut values)?;
            let index = if query.raw {0} else {let Some(i) = destination(&query.ranges,cursor.time,false) else {continue}; i};
            let unit = values[inputs].map(|n|n as i64);
            if query.weather.is_some() || query.raw {
                ensure!(units[index].is_none() || units[index] == unit,"unit system changes within computed series");
                if query.weather.is_some() { ensure!(unit == query.unit_system,"unit system changes within computed series"); }
            }
            units[index] = unit;
            let value = if let Some(weather) = &query.weather {
                state.evaluate(self,weather,cursor.time,&values[..inputs])?
            } else {values[0]};
            if query.raw {
                ensure!(answer.values.len() < self.workspace_bytes/64,"raw series workspace exceeded");
                let seconds = values[inputs+1].context("archive interval missing")?*60.0;
                ensure!(seconds.is_finite()&&seconds>=0.0&&seconds<i64::MAX as f64,"invalid archive interval");
                answer.starts.push(cursor.time.checked_sub(seconds as i64).context("archive interval overflow")?);
                answer.stops.push(cursor.time); answer.values.push(value.map_or(Json::Null,|v|json!(v)));
                reductions[0].rows += 1;
            } else {
                reductions[index].add(cursor.time,value,values[inputs+1].map(|v|v*60.0),&query.aggregate,query.threshold,&mut samples_left)?;
            }
        }
        if !query.raw {
            for reduction in &reductions { answer.values.push(reduction.finish(&query.aggregate,query.weather.is_some(),query.threshold)?); }
            if query.aggregate == "cumulative" {
                let mut sum = Accumulator::default();
                for value in &mut answer.values { sum.add(value.as_f64()); *value = json!(sum.clone().finish().sum.unwrap_or(0.0)); }
            }
        }
        answer.rows = reductions.iter().map(|r|r.rows).collect(); answer.units = units; answer.state = state.finish();
        Ok(answer)
    }

    fn daily_analytics(&mut self, query: &Query) -> Result<Answer> {
        let agg = query.aggregate.as_str();
        let historical = query.mode == "historical";
        let degree = query.mode == "degree";
        let weighted = degree || matches!(agg,"avg"|"historical_avg"|"avg_ge"|"avg_gt"|"avg_le"|"avg_lt"|"longest_ge"|"longest_lt"|"median"|"percentile"|"stddev"|"stddev_samp"|"variance"|"slope"|"maxgap");
        let names: Vec<String> = if weighted {vec!["wsum","sumtime"]}
            else {match agg {
                "rms"=>vec!["wsquaresum","sumtime"], "vecavg"=>vec!["xsum","ysum","sumtime"],
                "vecdir"=>vec!["xsum","ysum"], "windvec_avg"=>vec!["xsum","ysum","dirsumtime"],
                "count"|"not_null"=>vec!["count"],
                "min"|"maxmin"|"meanmin"|"historical_min"|"historical_min_avg"|"min_ge"|"min_le"=>vec!["min"],
                "max"|"minmax"|"meanmax"|"historical_max"|"historical_max_avg"|"max_ge"|"max_le"=>vec!["max"],
                "mintime"|"maxmintime"|"historical_mintime"=>vec!["min","mintime"],
                "maxtime"|"minmaxtime"|"historical_maxtime"=>vec!["max","maxtime"],
                "gustdir"=>vec!["max","max_dir"],
                _=>vec!["sum"],
            }}.into_iter().map(String::from).collect();
        let columns = indices(self,&query.table,&names)?;
        ensure!(columns.iter().all(Option::is_some),"daily statistic column missing");
        let mut bins = vec![DayBin::default();query.ranges.len()];
        let mut samples_left = self.workspace_bytes.saturating_sub(bins.len()*(std::mem::size_of::<DayBin>()+128))/32;
        let mut values = vec![None;names.len()];
        let start = if historical {i64::MIN} else {query.ranges.iter().map(|r|r.0).min().unwrap()};
        let stop = if historical {i64::MAX} else {query.ranges.iter().map(|r|r.1).max().unwrap()};
        let mut cursor = RowCursor::new(self,&query.table,Span {start,stop,daily:true})?;
        let mut lookup: HashMap<(i32,i32),Vec<usize>> = HashMap::new();
        if historical {
            ensure!(query.calendar_days.len() == bins.len(),"historical calendar length mismatch");
            for (i,day) in query.calendar_days.iter().enumerate() { lookup.entry(*day).or_default().push(i); }
        }
        while cursor.advance(self)? {
            numbers(self,&mut cursor,&columns,&mut values)?;
            let destinations: Vec<usize> = if historical {
                let (_,month,day,_,_) = crate::weather::date_parts(cursor.time,true)?;
                lookup.get(&(month,day)).cloned().unwrap_or_default()
            } else if degree {
                // Degree-day series may repeat a calendar day in adjacent
                // shorter output bins. Each result retains WeeWX's full day.
                query.ranges.iter().enumerate().filter_map(|(i,(start,stop))|(*start <= cursor.time && cursor.time < *stop).then_some(i)).collect()
            } else { destination(&query.ranges,cursor.time,true).into_iter().collect()
            };
            for i in destinations {
                let bin = &mut bins[i]; bin.rows += 1;
                bin.first.get_or_insert(cursor.time); bin.last = Some(cursor.time);
                if weighted {
                    let mean = values[0].zip(values[1]).and_then(|(sum,weight)|(weight != 0.0).then_some(sum/weight));
                    if degree {
                        let base = query.threshold.context("degree-day base missing")?;
                        let value = mean.map(|v|if query.observation == "heatdeg" {(base-v).max(0.0)} else {(v-base).max(0.0)});
                        bin.reduction.add(cursor.time,value,None,agg,None,&mut samples_left)?;
                    } else if agg.starts_with("avg_") {
                        if query.mode == "xagg_avg" {
                            if let Some(v) = mean { bin.a.add(Some(if compare(v,query.threshold,agg)? {1.0} else {0.0})); }
                        } else if values[1].is_some_and(|w|w != 0.0) {
                            bin.a.add(Some(if mean.is_some_and(|v|compare(v,query.threshold,agg).unwrap_or(false)) {1.0} else {0.0}));
                        }
                    } else if matches!(agg,"avg"|"historical_avg") {
                        bin.a.add(values[0]); bin.b.add(values[1]);
                    } else {
                        if bin.reduction.previous_time.is_some_and(|t|cursor.time.saturating_sub(t) > 36*3600) { bin.reduction.current_streak = 0; }
                        bin.reduction.add(cursor.time,mean,None,agg,query.threshold,&mut samples_left)?;
                    }
                } else if matches!(agg,"mintime"|"maxtime"|"maxmintime"|"minmaxtime"|"historical_mintime"|"historical_maxtime"|"gustdir"|"maxsumtime"|"minsumtime") {
                    let output = if agg.ends_with("sumtime") {Some(cursor.time as f64)} else {values[1]};
                    if !historical && matches!(agg,"mintime"|"maxtime"|"maxmintime"|"minmaxtime") && output.is_none() {continue;}
                    let descending = matches!(agg,"maxtime"|"maxmintime"|"historical_maxtime"|"gustdir"|"maxsumtime");
                    let tie = if historical || matches!(agg,"gustdir"|"maxsumtime"|"minsumtime") {Some(cursor.time as f64)} else {output};
                    bin.select(values[0],tie,output,descending);
                } else if agg.ends_with("_ge") || agg.ends_with("_le") {
                    bin.a.add(Some(if values[0].is_some_and(|v|compare(v,query.threshold,agg).unwrap_or(false)) {1.0} else {0.0}));
                } else {
                    bin.a.add(values[0]);
                    if values.len() > 1 {bin.b.add(values[1]);}
                    if values.len() > 2 {bin.c.add(values[2]);}
                }
            }
        }
        let mut answer = Answer::default();
        for bin in bins {
            let a = bin.a.finish(); let b = bin.b.finish(); let c = bin.c.finish();
            let value = if degree {bin.reduction.finish(agg,true,None)?}
                else if agg == "not_null" {if bin.rows == 0 {Json::Null} else {json!(a.max.is_some_and(|v|v > 0.0))}}
                else if matches!(agg,"mintime"|"maxtime"|"maxmintime"|"minmaxtime"|"historical_mintime"|"historical_maxtime"|"gustdir"|"maxsumtime"|"minsumtime") {
                    bin.selection.and_then(|(_,_,v)|v).map_or(Json::Null,|v|if agg == "gustdir" {json!(v)} else {json!(v as i64)})
                } else if weighted && !matches!(agg,"avg"|"historical_avg") && !agg.starts_with("avg_") {
                    bin.reduction.finish(agg,false,query.threshold)?
                } else {
                    let result = match agg {
                        "avg"|"historical_avg" => a.sum.zip(b.sum).and_then(|(a,b)|(b != 0.0).then_some(a/b)),
                        "rms" => a.sum.zip(b.sum).and_then(|(a,b)|(b != 0.0).then_some((a/b).sqrt())),
                        "vecavg" => a.sum.zip(b.sum).zip(c.sum).and_then(|((x,y),w)|(w != 0.0).then_some(((x*x+y*y)/(w*w)).sqrt())),
                        "vecdir" => a.sum.zip(b.sum).and_then(|(x,y)|vector_direction(x,y)),
                        "windvec_avg" => {
                            answer.values.push(a.sum.zip(b.sum).zip(c.sum).filter(|(_,w)|*w != 0.0).map_or(Json::Null,|((x,y),w)|json!([x/w,y/w])));
                            answer.rows.push(bin.rows); answer.units.push(query.unit_system); continue;
                        }
                        "min"|"minmax"|"minsum"|"historical_min" => a.min,
                        "max"|"maxmin"|"maxsum"|"historical_max" => a.max,
                        "meanmin"|"meanmax"|"historical_min_avg"|"historical_max_avg" => a.sum.map(|v|v/a.count as f64),
                        _ => a.sum,
                    };
                    result.map_or(Json::Null,|v|if agg == "count" || agg.ends_with("_ge") || agg.ends_with("_gt") || agg.ends_with("_le") || agg.ends_with("_lt") {json!(v as i64)} else {json!(v)})
                };
            answer.values.push(value); answer.rows.push(bin.rows); answer.units.push(query.unit_system);
            answer.starts.push(bin.first.unwrap_or(0)); answer.stops.push(bin.last.unwrap_or(0));
        }
        Ok(answer)
    }

    fn vector_analytics(&mut self, query: &Query) -> Result<Answer> {
        let weighted = query.mode == "wind";
        let (mag,dir) = if query.observation == "windgustvec" {("windGust","windGustDir")} else {("windSpeed","windDir")};
        let names = [mag,dir,"usUnits","interval"].map(String::from);
        let columns = indices(self,&query.table,&names)?;
        ensure!(columns[..2].iter().all(Option::is_some),"wind column missing");
        let mut cursor = RowCursor::new_bounds(self,&query.table,Some((query.ranges[0].0,query.raw)),Some((query.ranges.last().unwrap().1,true)))?;
        let mut bins = vec![VectorBin::default();query.ranges.len()];
        let mut values = [None;4]; let mut answer = Answer::default();
        while cursor.advance(self)? {
            numbers(self,&mut cursor,&columns,&mut values)?;
            let index = if query.raw {0} else {let Some(i) = destination(&query.ranges,cursor.time,false) else {continue}; i};
            let bin = &mut bins[index]; bin.rows += 1;
            let unit = values[2].map(|v|v as i64);
            let vector = match (values[0],values[1]) {
                (Some(0.0),_) => Some((0.0,0.0)),
                (Some(m),Some(d)) => {let angle = (90.0-d).to_radians();Some((m*angle.cos(),m*angle.sin()))},
                _=>None,
            };
            if weighted {
                if values[0].is_some() {bin.weight.add(values[3]);}
                if let (Some(m),Some(d),Some(w))=(values[0],values[1],values[3]){
                    let angle=(90.0-d).to_radians();bin.x.add(Some(w*m*angle.cos()));bin.y.add(Some(w*m*angle.sin()));bin.valid+=1;
                }
                continue;
            }
            if query.raw {
                ensure!(bin.units.is_none() || bin.units == unit,"unit system changes within computed series"); bin.units = unit;
                ensure!(answer.values.len() < self.workspace_bytes/80,"vector series workspace exceeded");
                answer.values.push(vector.map_or(Json::Null,|(x,y)|json!([x,y])));
                answer.starts.push(cursor.time-(values[3].unwrap_or(0.0)*60.0) as i64); answer.stops.push(cursor.time);
            } else if let Some(magnitude) = values[0] {
                bin.count += 1;
                if bin.first.is_none() {bin.first = Some((magnitude,vector,unit));}
                bin.last = Some((magnitude,vector,unit));
                if bin.min.is_none_or(|(m,_,_)|magnitude < m) {bin.min = Some((magnitude,vector,unit));}
                if bin.max.is_none_or(|(m,_,_)|magnitude > m) {bin.max = Some((magnitude,vector,unit));}
                if let Some((x,y)) = vector {
                    if matches!(query.aggregate.as_str(),"sum"|"avg") {ensure!(bin.units.is_none() || bin.units == unit,"unit system changes within computed series");}
                    bin.units = unit;
                    bin.x.add(Some(x)); bin.y.add(Some(y)); bin.valid += 1;
                }
            }
        }
        for bin in bins {
            if weighted {
                let x=bin.x.finish().sum;let y=bin.y.finish().sum;let w=bin.weight.finish().sum;
                let value=x.zip(y).and_then(|(x,y)|if query.aggregate=="vecdir" {vector_direction(x,y)} else {w.filter(|w|*w!=0.0).map(|w|((x*x+y*y)/(w*w)).sqrt())});
                answer.values.push(value.map_or(Json::Null,|v|json!(v)));answer.units.push(query.unit_system);answer.rows.push(bin.rows);continue;
            }
            if !query.raw {
                let selected = match query.aggregate.as_str() {"first"=>bin.first,"last"=>bin.last,"min"=>bin.min,"max"=>bin.max,_=>None};
                let value = match query.aggregate.as_str() {
                    "count"=>json!(bin.count), "not_null"=>json!(bin.count > 0),
                    "sum"|"avg"=>if bin.valid == 0 {Json::Null} else {
                        let divisor = if query.aggregate == "avg" {bin.valid as f64} else {1.0};
                        json!([bin.x.finish().sum.unwrap()/divisor,bin.y.finish().sum.unwrap()/divisor])
                    },
                    _=>selected.and_then(|(_,v,_)|v).map_or(Json::Null,|(x,y)|json!([x,y])),
                };
                answer.values.push(value);
                answer.units.push(if matches!(query.aggregate.as_str(),"sum"|"avg") {bin.units} else {selected.and_then(|(_,_,u)|u).or(query.unit_system)});
            } else {answer.units.push(bin.units);}
            answer.rows.push(bin.rows);
        }
        Ok(answer)
    }

    fn archive_special(&mut self, query:&Query)->Result<Answer>{
        let mut answer=Answer::default();
        let columns=self.columns(&query.table)?;
        let find=|name:&str|columns.iter().position(|c|c.name==name).context("unknown archive column");
        if query.mode=="gustdir" {
            let mag=find("windGust")?;let direction=find("windGustDir")?;
            for &(start,stop) in &query.ranges {
                let best=self.aggregate(&query.table,"windGust",Span{start,stop,daily:false})?.max;
                let mut cursor=RowCursor::new(self,&query.table,Span{start,stop,daily:false})?;
                if let Some(best)=best {cursor.pruning.push((mag,"eq".into(),Some(best)));}
                let mut result=Json::Null;let mut rows=0;
                while cursor.advance(self)? {let value=cursor.cell(self,mag)?.numeric();if value==best {result=cursor.cell(self,direction)?.json();rows=1;break;}}
                answer.values.push(result);answer.rows.push(rows);answer.units.push(query.unit_system);
            }
        } else {
            let column=find(&query.observation)?;
            for &(start,stop) in &query.ranges {
                let mut first=RowCursor::new_bounds(self,&query.table,Some((start,true)),None)?;
                let a=if first.advance(self)? {Some((first.time,first.cell(self,column)?))} else {None};
                let last=self.aggregate(&query.table,"dateTime",Span{start:i64::MIN,stop,daily:false})?.max;
                let b=if let Some(time)=last {
                    let time=time as i64;let mut cursor=RowCursor::new_bounds(self,&query.table,Some((time,true)),Some((time,true)))?;
                    if cursor.advance(self)? {Some((time,cursor.cell(self,column)?))} else {None}
                }else{None};
                let result=match(a,b){
                    (Some((ta,va)),Some((tb,vb)))=>{
                        if let (Cell::Integer(a),Cell::Integer(b))=(&va,&vb) {
                            let diff=*b as i128-*a as i128;let dt=tb as i128-ta as i128;
                            if query.aggregate=="diff" {i64::try_from(diff).map_or_else(|_|json!(diff as f64),|n|json!(n))}
                            else if dt==0 {Json::Null}
                            else if i64::try_from(diff).is_ok() {json!((diff/dt) as i64)}else{json!(diff as f64/dt as f64)}
                        }else{va.numeric().zip(vb.numeric()).and_then(|(a,b)|if query.aggregate=="diff" {Some(b-a)}else{let dt=tb as i128-ta as i128;(dt!=0).then_some((b-a)/dt as f64)}).map_or(Json::Null,|v|json!(v))}
                    },_=>Json::Null,
                };
                answer.values.push(result);answer.rows.push(u64::from(last.is_some()));answer.units.push(query.unit_system);
            }
        }
        Ok(answer)
    }
}

#[derive(Clone,Default)]
struct DayBin {
    rows: u64, a: Accumulator, b: Accumulator, c: Accumulator, reduction: Reduction,
    selection: Option<(Option<f64>,Option<f64>,Option<f64>)>, first: Option<i64>, last: Option<i64>,
}

impl DayBin {
    fn select(&mut self, value: Option<f64>, tie: Option<f64>, output: Option<f64>, descending: bool) {
        if self.selection.is_none_or(|(prior,old_tie,_)| {
            (if descending {value > prior} else {value < prior}) || (value == prior && tie < old_tie)
        }) { self.selection = Some((value,tie,output)); }
    }
}

type VectorSelection = Option<(f64,Option<(f64,f64)>,Option<i64>)>;
#[derive(Clone,Default)]
struct VectorBin {
    rows: u64, count: u64, valid: u64, x: Accumulator, y: Accumulator, units: Option<i64>,
    first: VectorSelection, last: VectorSelection, min: VectorSelection, max: VectorSelection, weight:Accumulator,
}

fn compare(value: f64, target: Option<f64>, aggregate: &str) -> Result<bool> {
    let target = target.context("threshold value missing")?;
    Ok(if aggregate.ends_with("_ge") {value >= target} else if aggregate.ends_with("_gt") {value > target}
        else if aggregate.ends_with("_le") {value <= target} else {value < target})
}

pub fn vector_direction(x: f64, y: f64) -> Option<f64> {
    if x == 0.0 && y == 0.0 {None} else {let degrees = 90.0-y.atan2(x).to_degrees();Some(if degrees < 0.0 {degrees+360.0} else {degrees})}
}
