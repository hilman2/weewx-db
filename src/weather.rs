//! WeeWX weather kernels. Unit conversion constants follow the Python reference.

use crate::*;
use std::collections::BTreeMap;

pub const METER_PER_MILE: f64 = 1609.34;
pub const METER_PER_FOOT: f64 = METER_PER_MILE/5280.0;
pub const INHG_PER_MBAR: f64 = 0.0295299875;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Parameters {
    #[serde(flatten)] pub values: BTreeMap<String,serde_json::Value>,
}

impl Parameters {
    pub fn number(&self, name: &str, default: f64) -> f64 { self.values.get(name).and_then(|v|v.as_f64()).unwrap_or(default) }
    pub fn text<'a>(&'a self, name: &str, default: &'a str) -> &'a str { self.values.get(name).and_then(|v|v.as_str()).unwrap_or(default) }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WeatherSpec {
    pub formula: String,
    pub inputs: Vec<String>,
    pub unit_system: i64,
    #[serde(default)] pub parameters: Parameters,
}

pub fn celsius(t: f64, units: i64) -> f64 { if units == 1 { (t-32.0)/1.8 } else { t } }
pub fn fahrenheit(t: f64, units: i64) -> f64 { if units == 1 { t } else { t*1.8+32.0 } }
pub fn mps(v: f64, units: i64) -> f64 { match units {1=>v*METER_PER_MILE/3600.0,16=>v*0.277777778,_=>v} }
pub fn finite(value: f64) -> Option<f64> { value.is_finite().then_some(value) }

impl WeatherSpec {
    pub fn value(&self, time: i64, args: &[Option<f64>]) -> Option<f64> {
        let a = |i:usize|args.get(i).copied().flatten();
        let units = self.unit_system;
        let p = &self.parameters;
        let formula = match self.formula.as_str() {
            "dewpoint"=>Some(crate::computed::Formula::Dewpoint),"humidex"=>Some(crate::computed::Formula::Humidex),
            "heatindex"=>Some(crate::computed::Formula::Heatindex),"heatindex_old"=>Some(crate::computed::Formula::HeatindexOld),_=>None,
        };
        if let Some(formula) = formula {
            return crate::computed::Computed {formula,temperature:String::new(),humidity:String::new(),unit_system:units}.value(a(0),a(1));
        }
        let result = match self.formula.as_str() {
            "identity" => a(0)?,
            "null" => return None,
            "windchill" => {
                let t = fahrenheit(a(0)?,units);
                let v = match units {1=>a(1)?,16=>0.621371192*a(1)?,_=>2.237*a(1)?};
                let f = if t >= 50.0 || v <= 3.0 { t } else {35.74+0.6215*t+(-35.75+0.4275*t)*v.powf(0.16)};
                if units == 1 { f } else { (f-32.0)/1.8 }
            }
            "appTemp" => {
                let t = celsius(a(0)?,units); let h = a(1)?; let v = mps(a(2)?,units);
                if !(0.0..=100.0).contains(&h) || v < 0.0 { return None; }
                let e = h/100.0*6.105*(17.27*t/(237.7+t)).exp();
                let c = t+0.33*e-0.7*v-4.0;
                if units == 1 { c*1.8+32.0 } else { c }
            }
            "cloudbase" => {
                let altitude = p.number("altitude_m",0.0);
                if units == 1 {
                    let t = a(0)?; let dp = crate::computed::dewpoint_c((t-32.0)/1.8,a(1)?)?*1.8+32.0;
                    altitude/METER_PER_FOOT+(t-dp)*1000.0/4.4
                } else {
                    let t = a(0)?; let dp = crate::computed::dewpoint_c(t,a(1)?)?;
                    altitude+(t-dp)*1000.0/2.5*METER_PER_FOOT
                }
            }
            "beaufort" => {
                let speed = a(0)?*match units {1=>0.868976242,16=>0.539956803,_=>1.94384449};
                [1.0,4.0,7.0,11.0,17.0,22.0,28.0,34.0,41.0,48.0,56.0,64.0].partition_point(|v|speed.abs() >= *v) as f64
            }
            "windrun" => {
                let v = a(0)?; let minutes = a(1)?;
                if units == 17 { v*minutes*60.0/1000.0 } else { v*minutes/60.0 }
            }
            "barometer" => {
                let pressure = a(0)?; let t = celsius(a(1)?,units);
                let converted = if units == 1 { pressure/INHG_PER_MBAR } else { pressure };
                let term = (-p.number("altitude_m",0.0)/((t+273.15)*29.263)).exp();
                let result = if term != 0.0 { converted/term } else {0.0};
                if units == 1 { result*INHG_PER_MBAR } else { result }
            }
            "altimeter" => {
                let pressure = a(0)?;
                if pressure <= if units == 1 {0.008859} else {0.3} { return None; }
                let pressure = if units == 1 { pressure/0.02953 } else { pressure };
                let altitude = p.number("altitude_m",0.0);
                // uWxUtils uses 0.3048; WeeWX's unit converter uses METER_PER_FOOT.
                let altitude = if units == 1 { altitude/METER_PER_FOOT*0.3048 } else { altitude };
                let value = altimeter(pressure,altitude,p.text("altimeter_algorithm","aaASOS"))?;
                if units == 1 { value*0.02953 } else { value }
            }
            "pressure" => sensor_pressure(a(0)?,a(1)?,a(2)?,a(3)?,units,p.number("altitude_m",0.0)),
            "delta" => { let new = a(0)?; let old = a(1)?; if new < old { return None; } new-old },
            "solar_rs"|"solar_bras" => return crate::solar::radiation(time,p,&self.formula),
            _ => return None,
        };
        finite(result)
    }
}

fn altimeter(pressure: f64, elevation: f64, algorithm: &str) -> Option<f64> {
    let geop = 6356766.0*elevation/(6356766.0+elevation);
    let k = 0.0065*(8.31432/0.0289644)/9.80665;
    let value = match algorithm {
        "aaASOS" => ((pressure*0.02953).powf(0.1903)+1.313e-5*elevation/0.3048).powf(5.255)/0.02953,
        "aaASOS2" => (pressure.powf(k)+8.41728638e-5*geop).powf(1.0/k),
        "aaMADIS" => ((pressure-0.3).powf(0.190284)+8.4184960528e-5*elevation).powf(1.0/0.190284),
        "aaNOAA" => (pressure-0.3)*(1.0+8.42288069e-5*(elevation/(pressure-0.3).powf(0.190284))).powf(1.0/0.190284),
        "aaWOB" => ((pressure*0.02953).powf(k)+1.312603e-5*elevation/0.3048).powf(1.0/k)/0.02953,
        "aaSMT" => {
            let p = pressure*0.02953-0.01;
            p*(1.0+4.30899e-5*geop/p.powf(0.190284)).powf(1.0/0.190284)/0.02953
        }
        _ => return None,
    };
    finite(value)
}

fn sensor_pressure(t: f64, barometer: f64, humidity: f64, previous_f: f64, units: i64, altitude: f64) -> f64 {
    let current_f = fahrenheit(t,units);
    let pressure_in = if units == 1 { barometer } else { barometer*INHG_PER_MBAR };
    let feet = altitude/METER_PER_FOOT;
    let elevation = feet*0.3048;
    let current_c = (current_f-32.0)*5.0/9.0;
    let rounded = (((current_f-0.01).round_ties_even()+(previous_f-0.01).round_ties_even())/2.0-0.01).round_ties_even();
    let mean_c = (rounded-32.0)*5.0/9.0;
    let mean_f = mean_c*9.0/5.0+32.0;
    let correction = if humidity > 0.0 {
        1.8*(humidity*0.01*6.112*(17.62*current_c/(243.12+current_c)).exp())
            *(2.8322e-9*elevation.powi(2)+2.225e-5*elevation+0.10743)
    } else {0.0};
    pressure_in/10.0_f64.powf(feet/(122.8943111*(mean_f+460.0+feet*0.00275/2.0+correction)))
}

pub fn date_parts(time: i64, local: bool) -> Result<(i32,i32,i32,i32,f64)> {
    let timestamp = libc::time_t::try_from(time).context("timestamp outside platform range")?;
    let mut parts = std::mem::MaybeUninit::<libc::tm>::uninit();
    #[cfg(unix)]
    let ptr = unsafe { if local { libc::localtime_r(&timestamp,parts.as_mut_ptr()) } else { libc::gmtime_r(&timestamp,parts.as_mut_ptr()) } };
    #[cfg(not(unix))]
    let ptr = unsafe { if local { libc::localtime(&timestamp) } else { libc::gmtime(&timestamp) } };
    ensure!(!ptr.is_null(),"timestamp outside calendar range");
    // The libc conversion succeeded, so all tm fields are initialized.
    let p = unsafe { *ptr };
    Ok((p.tm_year+1900,p.tm_mon+1,p.tm_mday,p.tm_yday,p.tm_hour as f64+p.tm_min as f64/60.0+p.tm_sec as f64/3600.0))
}

pub fn evapotranspiration(args: &[f64;6], time: i64, units: i64, p: &Parameters) -> Option<f64> {
    let [tmin,tmax,hmin,hmax,radiation,wind] = *args;
    // ETXType converts through US units before calling the metric formula.
    let tmin = (fahrenheit(tmin,units)-32.0)/1.8;
    let tmax = (fahrenheit(tmax,units)-32.0)/1.8;
    let wind_mph = match units {1=>wind,16=>wind*1000.0/METER_PER_MILE,_=>wind*3600.0/METER_PER_MILE};
    let wind = wind_mph*METER_PER_MILE/3600.0;
    let altitude = p.number("altitude_m",0.0)/METER_PER_FOOT*METER_PER_FOOT;
    let height = p.number("wind_height",2.0)/METER_PER_FOOT*METER_PER_FOOT;
    let doy = date_parts(time,true).ok()?.3 as f64;
    let hour = date_parts(time,false).ok()?.4;
    let average = (tmax+tmin)/2.0; let humidity = (hmin+hmax)/2.0;
    let u2 = 4.87*wind/(67.8*height-5.42).ln();
    let pressure = 101.3*((293.0-0.0065*altitude)/293.0).powf(5.26);
    let gamma = 0.665e-3*pressure;
    let etmin = 6.1078*10.0_f64.powf(7.5*tmin/(tmin+237.3))/10.0;
    let etmax = 6.1078*10.0_f64.powf(7.5*tmax/(tmax+237.3))/10.0;
    let e0 = (etmin+etmax)/2.0;
    let delta = 4098.0*(0.6108*(17.27*average/(average+237.3)).exp())/(average+237.3).powi(2);
    let ea = (etmin*hmax+etmax*hmin)/200.0;
    let rs = radiation*3.6e-3;
    let rns = (1.0-p.number("albedo",0.23))*rs;
    let ra = sun_radiation(doy,p.number("latitude",0.0),p.number("longitude",0.0),hour);
    let rso = (0.75+2e-5*altitude)*ra;
    let cloud = if rso != 0.0 { rs/rso } else if humidity > 80.0 {0.3} else if humidity > 40.0 {0.5} else {0.8};
    let rnl = 4.903e-9*((tmin+273.16).powi(4)+(tmax+273.16).powi(4))/2.0
        *(0.34-0.14*ea.sqrt())*(1.35*cloud-0.35)/24.0;
    let rn = rns-rnl; let ground = if rs != 0.0 {0.1*rn} else {0.5*rn};
    let rate = (0.408*delta*(rn-ground)+gamma*(p.number("cn",37.0)/(average+273.0))*u2*(e0-ea))
        /(delta+gamma*(1.0+p.number("cd",0.34)*u2));
    finite(rate.max(0.0)/25.4)
}

fn sun_radiation(day: f64, latitude: f64, longitude: f64, hour: f64) -> f64 {
    let b = 2.0*std::f64::consts::PI*(day-81.0)/364.0;
    let correction = 0.1645*(2.0*b).sin()-0.1255*b.cos()-0.025*b.sin();
    let angle = |hour:f64| { let a = std::f64::consts::PI/12.0*(hour+longitude/15.0+correction-12.0); if a < 0.0 { a+std::f64::consts::TAU } else { a } };
    let delta = 0.409*(std::f64::consts::TAU*day/365.0-1.39).sin();
    let distance = 1.0+0.033*(std::f64::consts::TAU*day/365.0).cos();
    let lat = latitude.to_radians(); let first = angle(hour-1.0); let last = angle(hour);
    (12.0/std::f64::consts::PI*4.92*distance*((last-first)*lat.sin()*delta.sin()+lat.cos()*delta.cos()*(last.sin()-first.sin()))).max(0.0)
}
