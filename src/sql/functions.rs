use super::*;
use rusqlite::functions::{Aggregate, WindowAggregate, Context, FunctionFlags};

#[derive(Default)]
struct SumState { count: u64, reals: u64, integer: i128, sum: f64, correction: f64 }

struct Numeric { average: bool }

impl SumState {
    fn add(&mut self, value: ValueRef<'_>, inverse: bool) -> rusqlite::Result<()> {
        if value == ValueRef::Null { return Ok(()); }
        let value = match value {
            ValueRef::Integer(n) => { self.integer += if inverse { -(n as i128) } else { n as i128 }; n as f64 },
            ValueRef::Real(n) => { if inverse { self.reals -= 1; } else { self.reals += 1; } n },
            _ => return Err(error("numeric aggregate requires numeric input")),
        };
        if inverse { self.count -= 1; } else { self.count += 1; }
        let value = if inverse { -value } else { value };
        let next = self.sum + value;
        self.correction += if self.sum.abs() >= value.abs() { (self.sum-next)+value } else { (value-next)+self.sum };
        self.sum = next;
        if self.count == 0 { *self = Self::default(); }
        Ok(())
    }
}

impl Numeric {
    fn result(&self, state: Option<&SumState>) -> rusqlite::Result<Value> {
        let Some(state) = state.filter(|s| s.count > 0) else { return Ok(Value::Null) };
        if self.average { return Ok(Value::Real((state.sum + state.correction) / state.count as f64)); }
        if state.reals == 0 { return Ok(Value::Integer(i64::try_from(state.integer).map_err(|_| error("integer overflow"))?)); }
        Ok(Value::Real(state.sum + state.correction))
    }
}

impl Aggregate<SumState, Value> for Numeric {
    fn init(&self, _: &mut Context<'_>) -> rusqlite::Result<SumState> { Ok(SumState::default()) }
    fn step(&self, context: &mut Context<'_>, state: &mut SumState) -> rusqlite::Result<()> { state.add(context.get_raw(0), false) }
    fn finalize(&self, _: &mut Context<'_>, state: Option<SumState>) -> rusqlite::Result<Value> { self.result(state.as_ref()) }
}

impl WindowAggregate<SumState, Value> for Numeric {
    fn value(&self, state: Option<&mut SumState>) -> rusqlite::Result<Value> { self.result(state.as_deref()) }
    fn inverse(&self, context: &mut Context<'_>, state: &mut SumState) -> rusqlite::Result<()> { state.add(context.get_raw(0), true) }
}

#[derive(Default)]
struct Moments { count: u64, mean: f64, m2: f64 }
struct Variance { sample: bool, root: bool }
impl Aggregate<Moments, Option<f64>> for Variance {
    fn init(&self, _: &mut Context<'_>) -> rusqlite::Result<Moments> { Ok(Moments::default()) }
    fn step(&self, context: &mut Context<'_>, state: &mut Moments) -> rusqlite::Result<()> {
        if let Some(value) = context.get::<Option<f64>>(0)? {
            state.count += 1;
            let delta = value - state.mean;
            state.mean += delta / state.count as f64;
            state.m2 += delta * (value-state.mean);
        }
        Ok(())
    }
    fn finalize(&self, _: &mut Context<'_>, state: Option<Moments>) -> rusqlite::Result<Option<f64>> {
        let Some(state) = state.filter(|s| s.count > u64::from(self.sample)) else { return Ok(None) };
        let variance = (state.m2 / (state.count-u64::from(self.sample)) as f64).max(0.0);
        Ok(Some(if self.root { variance.sqrt() } else { variance }))
    }
}

#[derive(Default)]
struct PercentileState { values: Vec<f64>, percentile: Option<f64> }
struct Percentile { limit: usize }
impl Aggregate<PercentileState, Option<f64>> for Percentile {
    fn init(&self, _: &mut Context<'_>) -> rusqlite::Result<PercentileState> { Ok(PercentileState::default()) }
    fn step(&self, context: &mut Context<'_>, state: &mut PercentileState) -> rusqlite::Result<()> {
        let p = context.get::<f64>(1)?;
        ensure_sql(p.is_finite() && (0.0..=1.0).contains(&p), "percentile must be between 0 and 1")?;
        ensure_sql(state.percentile.is_none_or(|previous| previous == p), "percentile must be constant within a group")?;
        state.percentile = Some(p);
        if let Some(value) = context.get::<Option<f64>>(0)? {
            ensure_sql(state.values.len() < self.limit, "exact percentile workspace exceeded; increase workspace_mib or restrict the time span")?;
            state.values.push(value);
        }
        Ok(())
    }
    fn finalize(&self, _: &mut Context<'_>, state: Option<PercentileState>) -> rusqlite::Result<Option<f64>> {
        let Some(mut state) = state.filter(|s| !s.values.is_empty()) else { return Ok(None) };
        let position = state.percentile.unwrap() * (state.values.len()-1) as f64;
        let low = position.floor() as usize;
        let high = position.ceil() as usize;
        let lower = *state.values.select_nth_unstable_by(low, f64::total_cmp).1;
        let upper = if low == high { lower } else { *state.values.select_nth_unstable_by(high, f64::total_cmp).1 };
        Ok(Some(lower + (upper-lower) * position.fract()))
    }
}

#[derive(Default)]
struct VectorState { x: f64, y: f64, weight: f64 }
struct Vector { direction: bool }
impl Aggregate<VectorState, Option<f64>> for Vector {
    fn init(&self, _: &mut Context<'_>) -> rusqlite::Result<VectorState> { Ok(VectorState::default()) }
    fn step(&self, context: &mut Context<'_>, state: &mut VectorState) -> rusqlite::Result<()> {
        let speed = context.get::<Option<f64>>(0)?;
        let direction = context.get::<Option<f64>>(1)?;
        let weight = context.get::<Option<f64>>(2)?;
        if let (Some(0.0),Some(weight)) = (speed,weight) {
            ensure_sql(weight >= 0.0,"wind vector weight must be nonnegative")?;
            state.weight += weight;
            return Ok(());
        }
        if let (Some(speed), Some(direction), Some(weight)) = (speed, direction, weight) {
            ensure_sql(weight >= 0.0, "wind vector weight must be nonnegative")?;
            let angle = (90.0-direction).to_radians();
            state.x += weight * speed * angle.cos();
            state.y += weight * speed * angle.sin();
            state.weight += weight;
        }
        Ok(())
    }
    fn finalize(&self, _: &mut Context<'_>, state: Option<VectorState>) -> rusqlite::Result<Option<f64>> {
        let Some(s) = state.filter(|s| s.weight > 0.0) else { return Ok(None) };
        Ok(if self.direction {
            if s.x == 0.0 && s.y == 0.0 { None } else { Some((90.0-s.y.atan2(s.x).to_degrees()).rem_euclid(360.0)) }
        } else { Some(s.x.hypot(s.y) / s.weight) })
    }
}

pub(super) fn register(connection: &Connection, workspace: usize) -> Result<()> {
    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC | FunctionFlags::SQLITE_INNOCUOUS;
    connection.create_window_function("sum", 1, flags, Numeric { average: false })?;
    connection.create_window_function("avg", 1, flags, Numeric { average: true })?;
    for (name, sample, root) in [("stddev_pop",false,true),("stddev_samp",true,true),("variance_pop",false,false),("variance_samp",true,false)] {
        connection.create_aggregate_function(name,1,flags,Variance { sample,root })?;
    }
    connection.create_aggregate_function("percentile_cont",2,flags,Percentile {limit:workspace/8})?;
    connection.create_aggregate_function("wind_vec_avg",3,flags,Vector { direction:false })?;
    connection.create_aggregate_function("wind_vec_dir",3,flags,Vector { direction:true })?;
    connection.create_scalar_function("time_bucket",2,flags,|ctx| {
        let time: Option<i64> = ctx.get(0)?;
        let seconds: Option<i64> = ctx.get(1)?;
        let (Some(time),Some(seconds)) = (time,seconds) else { return Ok(None) };
        ensure_sql(seconds > 0, "bucket width must be positive")?;
        let bucket = (time as i128).div_euclid(seconds as i128) * seconds as i128;
        i64::try_from(bucket).map(Some).map_err(error)
    })?;
    for (name,formula) in [("dewpoint",crate::computed::Formula::Dewpoint),
        ("humidex",crate::computed::Formula::Humidex),("heatindex",crate::computed::Formula::Heatindex),
        ("heatindex_old",crate::computed::Formula::HeatindexOld)] {
        for (suffix,unit_system) in [("c",17),("f",1)] {
            let source = crate::computed::Computed {formula,temperature:String::new(),humidity:String::new(),unit_system};
            connection.create_scalar_function(format!("{name}_{suffix}").as_str(),2,flags,move|ctx| {
                Ok(source.value(ctx.get::<Option<f64>>(0)?,ctx.get::<Option<f64>>(1)?))
            })?;
        }
    }
    connection.create_scalar_function("wx_value",-1,flags,|ctx| {
        ensure_sql(ctx.len()>=4,"wx_value expects formula, timestamp, units, parameters JSON, then inputs")?;
        let formula:String=ctx.get(0)?;
        ensure_sql(matches!(formula.as_str(),"dewpoint"|"humidex"|"heatindex"|"heatindex_old"|"windchill"|"appTemp"|"cloudbase"|"beaufort"|"windrun"|"barometer"|"altimeter"|"pressure"|"solar_rs"|"solar_bras"),"wx_value requires a stateless weather formula")?;
        let Some(time)=ctx.get::<Option<i64>>(1)? else{return Ok(None)};
        let Some(unit_system)=ctx.get::<Option<i64>>(2)? else{return Ok(None)};
        let parameters=serde_json::from_str(&ctx.get::<String>(3)?).map_err(error)?;
        let mut inputs=Vec::with_capacity(ctx.len()-4);
        for i in 4..ctx.len(){inputs.push(ctx.get::<Option<f64>>(i)?);}
        let weather=crate::weather::WeatherSpec{formula,inputs:vec![],unit_system,parameters};
        Ok(weather.value(time,&inputs))
    })?;
    connection.create_scalar_function("astro_position",5,flags,|ctx| {
        let body:String=ctx.get(0)?;
        let Some(time)=ctx.get::<Option<f64>>(1)? else{return Ok(None)};
        let observer=crate::astro::Observer{latitude:ctx.get(2)?,longitude:ctx.get(3)?,elevation:ctx.get(4)?,pressure:1010.0,temperature:15.0,horizon:0.0};
        let position=crate::astro::position(&body,time,&observer).map_err(error)?;
        serde_json::to_string(&position).map(Some).map_err(error)
    })?;
    connection.create_scalar_function("astro_events",8,flags,|ctx| {
        let body:String=ctx.get(0)?;let start:f64=ctx.get(1)?;let stop:f64=ctx.get(2)?;
        let observer=crate::astro::Observer{latitude:ctx.get(3)?,longitude:ctx.get(4)?,elevation:ctx.get(5)?,pressure:1010.0,temperature:15.0,horizon:ctx.get(6)?};
        let result=crate::astro::events(&body,start,stop,&observer,ctx.get(7)?).map_err(error)?;
        serde_json::to_string(&result).map_err(error)
    })?;
    Ok(())
}
