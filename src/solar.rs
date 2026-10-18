use crate::weather::{Parameters,finite};

pub fn radiation(time: i64, parameters: &Parameters, formula: &str) -> Option<f64> {
    if parameters.number("solar_enabled",1.0) == 0.0 { return None; }
    let observer=crate::astro::Observer {latitude:parameters.number("latitude",0.0),longitude:parameters.number("longitude",0.0),
        elevation:parameters.number("altitude_m",0.0),pressure:1010.0,temperature:15.0,horizon:0.0};
    let position=crate::astro::position("sun",time as f64,&observer).ok()?;
    // WeeWX's PyEphem reference exposes these two quantities at f32 precision.
    let elevation=((position.altitude.to_radians() as f32) as f64).to_degrees();
    let distance=(position.distance_au as f32) as f64;
    let sine = elevation.to_radians().sin();
    if sine < 0.0 { return Some(0.0); }
    let optical = sine+0.15*(elevation+3.885).powf(-1.253);
    let top = 1367.0*sine/(distance*distance);
    let result = if formula == "solar_bras" {
        let mass = 1.0/optical;
        let scattering = 0.128-0.054*mass.ln()/10.0_f64.ln();
        top*(-parameters.number("nfac",2.0)*scattering*mass).exp()
    } else {
        let mass = ((288.0-0.0065*parameters.number("altitude_m",0.0))/288.0).powf(5.256)/optical;
        let atc = parameters.number("atc",0.8);
        let atc = if (0.7..=0.91).contains(&atc) {atc} else {0.8};
        top*atc.powf(mass)
    };
    finite(result)
}
