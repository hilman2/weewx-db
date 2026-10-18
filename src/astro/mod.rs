//! Solar and lunar astronomy in Rust, using PyEphem's numerical reference models.

use crate::*;
use std::f64::consts::{PI,TAU};
mod data;
mod lunar;
mod lunar_expressions;
mod events;
pub use events::{events,phase_events};
use data::*;

const J2000: f64 = 36525.0;
const EARTH_RADIUS: f64 = 6.37816e6;
const AU: f64 = 1.4959787e11;

#[derive(Clone,Debug,Deserialize,Serialize)]
pub struct Observer {
    pub latitude: f64,
    pub longitude: f64,
    #[serde(default)] pub elevation: f64,
    #[serde(default="default_pressure")] pub pressure: f64,
    #[serde(default="default_temperature")] pub temperature: f64,
    #[serde(default)] pub horizon: f64,
}
fn default_pressure()->f64{1010.0}
fn default_temperature()->f64{15.0}

#[derive(Clone,Debug,Serialize)]
pub struct Position {
    pub altitude: f64, pub azimuth: f64, pub distance_au: f64,
    pub right_ascension: f64, pub declination: f64, pub hour_angle: f64,
    pub geocentric_longitude: f64, pub geocentric_latitude: f64,
    pub angular_radius: f64, pub illumination: f64, pub phase_angle: f64,
    pub moon_fullness: f64, pub geocentric_ra: f64, pub geocentric_dec: f64,
}

#[derive(Deserialize)]
pub struct Request {
    pub time:f64,
    #[serde(flatten)] pub observer:Observer,
    #[serde(default="default_body")] pub body:String,
    #[serde(default="default_operation")] pub operation:String,
    #[serde(default)] pub start:Option<f64>,
    #[serde(default)] pub stop:Option<f64>,
    #[serde(default)] pub center:bool,
}
fn default_body()->String{String::from("sun")}
fn default_operation()->String{String::from("position")}

pub fn execute(request:&Request)->Result<serde_json::Value>{
    let start=request.start.unwrap_or(request.time);
    match request.operation.as_str(){
        "position"=>Ok(serde_json::to_value(position(&request.body,request.time,&request.observer)?)?),
        "sidereal"=>{validate(request.time,&request.observer)?;Ok(serde_json::json!(degrees(sidereal(request.time,request.observer.longitude))))},
        "events"=>Ok(serde_json::to_value(events(&request.body,start,request.stop.unwrap_or(start+86400.0),&request.observer,request.center)?)?),
        "phases"=>Ok(serde_json::to_value(phase_events(start,request.stop.unwrap_or(start+32.0*86400.0))?)?),
        _=>bail!("unknown astronomy operation"),
    }
}

pub fn range(value: f64, period: f64) -> f64 { value-period*(value/period).floor() }
pub fn mod_arcsec(value: f64) -> f64 { range(value,1296000.0) }
fn radians(value:f64)->f64{value*PI/180.0}
fn degrees(value:f64)->f64{value*180.0/PI}

fn validate(time: f64, observer: &Observer) -> Result<()> {
    ensure!(time.is_finite() && (-90.0..=90.0).contains(&observer.latitude)
        && observer.longitude.is_finite() && observer.elevation.is_finite()
        && observer.pressure.is_finite() && observer.pressure >= 0.0
        && observer.temperature.is_finite() && observer.temperature > -273.0
        && observer.horizon.is_finite(),"invalid astronomical time or observer");
    let (year,_,_) = calendar(time/86400.0+25567.5);
    ensure!((-1000..=2900).contains(&year),"astronomical reference range is -1000 through 2900");
    Ok(())
}

fn calendar(mjd: f64) -> (i32,i32,f64) {
    let day = mjd+0.5; let mut whole = day.floor(); let fraction = day-whole;
    if whole > -115860.0 { let a = (whole/36524.25+0.99835726).floor()+14.0; whole += 1.0+a-(a/4.0).floor(); }
    let b = (whole/365.25+0.802601).floor();
    let c = whole-(365.25*b+0.750001).floor()+416.0;
    let g = (c/30.6001).floor();
    let month = if g > 13.5 {(g-13.0) as i32} else {(g-1.0) as i32};
    let mut year = if month < 3 {(b+1900.0) as i32} else {(b+1899.0) as i32};
    if year < 1 {year -= 1;}
    (year,month,c-(30.6001*g).floor()+fraction)
}

fn calendar_mjd(year: i32, month: i32, day: f64) -> f64 {
    let mut y = if year < 0 {year+1} else {year}; let mut m = month;
    if m < 3 {m += 12; y -= 1;}
    let correction = if year < 1582 || year == 1582 && (month < 10 || month == 10 && day < 15.0) {0}
        else {let century=y/100;2-century+century/4};
    let c = (365.25*y as f64-if y < 0 {0.75} else {0.0}).trunc()-694025.0;
    correction as f64+c+(30.6001*(m+1) as f64).trunc()+day-0.5
}

pub fn delta_t(time: f64) -> f64 {
    let mjd = time/86400.0+25567.5;
    let (year,_,_) = calendar(mjd);
    let first = calendar_mjd(year,1,1.0); let last = calendar_mjd(year+1,1,1.0);
    let y = year as f64+(mjd-first)/(last-first);
    if y > 2018.0 {
        let big = (y-1820.0)*0.01;
        if y > 2118.0 {return 32.0*big*big-20.0;}
        let a = DELTA_TABLE[398] as f64*0.01;
        let b = (DELTA_TABLE[398]-DELTA_TABLE[388]) as f64*0.001;
        let future = (2118.0-1820.0)*0.01;
        let m0 = 32.0*future*future-20.0; let m1 = 0.64*future;
        let d = 2e-6*(50.0*(m1+b)-m0+a);
        let c = 1e-4*(m0-a-100.0*b-1e6*d); let p = y-2018.0;
        return a+p*(b+p*(c+p*d));
    }
    if y < 1700.0 {
        if y <= -1000.0 {let b=(y-1820.0)*0.01;return 32.0*b*b-20.0;}
        let index = ((y as i32+1000)/100) as usize; let base = -1000.0+100.0*index as f64;
        let p = DELTA_HISTORICAL[index] as f64;
        return p+0.01*(y-base)*(DELTA_HISTORICAL[index+1] as f64-p);
    }
    let index = (y.floor()-1620.0) as usize;
    let mut answer = DELTA_TABLE[index] as f64;
    if index+1 >= DELTA_TABLE.len() {return answer*0.01;}
    let p = y-y.floor(); answer += p*(DELTA_TABLE[index+1]-DELTA_TABLE[index]) as f64;
    if index == 0 || index+2 >= DELTA_TABLE.len() {return answer*0.01;}
    let mut differences = [0;5];
    for (i,difference) in differences.iter_mut().enumerate() {
        let k = index as isize-2+i as isize;
        if k >= 0 && (k as usize)+1 < DELTA_TABLE.len() {*difference=DELTA_TABLE[k as usize+1]-DELTA_TABLE[k as usize];}
    }
    for i in 0..4 {differences[i]=differences[i+1]-differences[i];}
    let mut b=0.25*p*(p-1.0); answer+=b*(differences[1]+differences[2]) as f64;
    for i in 0..3 {differences[i]=differences[i+1]-differences[i];}
    b=2.0*b/3.0; answer+=(p-0.5)*b*differences[1] as f64;
    if index >= 2 && index+3 <= DELTA_TABLE.len() {
        for i in 0..2 {differences[i]=differences[i+1]-differences[i];}
        b=0.125*b*(p+1.0)*(p-2.0);answer+=b*(differences[0]+differences[1]) as f64;
    }
    answer*0.01
}

fn obliquity(mjd:f64)->f64 {
    let t=(mjd-J2000)/36525.0;
    radians(23.4392911+t*(-46.8150+t*(-0.00059+t*0.001813))/3600.0)
}

fn nutation(mjd:f64)->(f64,f64) {
    let t=(mjd-J2000)/36525.0; let t2=t*t;let t3=t2*t;
    let mut cache=[[0.0;9];5];
    for (i,d) in DELAUNAY.iter().enumerate() {
        let mut x=(d[0]+d[1]*t+d[2]*t2+d[3]*t3)/1296000.0;x-=x.floor();x*=TAU;
        for (j,value) in cache[i].iter_mut().enumerate() {*value=(j as f64-4.0)*x;}
    }
    let (mut psi,mut eps)=(0.0,0.0);let mut secular=0;
    for (i,arguments) in NUT_ARGUMENTS.iter().enumerate() {
        let [a,b]=NUT_CONSTANT[i];
        let (sine,cosine)=if a != 0.0 || b != 0.0 {(a,b)} else {
            let row=NUT_SECULAR[secular];secular+=1;(row[1]+row[2]*(t/10.0),row[3]+row[4]*(t/10.0))
        };
        let mut argument=0.0;for j in 0..5 {argument+=cache[j][(4+arguments[j]) as usize];}
        psi+=sine*argument.sin();eps+=cosine*argument.cos();
    }
    (radians(eps/3600.0/1e4),radians(psi/3600.0/1e4))
}

fn solar_ecliptic(mjd:f64)->(f64,f64,f64) {
    let t=(mjd-J2000)/365250.0; let powers=[1.0,t,t*t,t*t*t,t*t*t*t,t*t*t*t*t];
    let mut result=[0.0;3];
    for coord in 0..3 {
        let mut alpha=0;
        while EARTH_INDEX[alpha+1][coord] != 0 {
            let mut term=0.0;
            for row in &EARTH[EARTH_INDEX[alpha][coord]..EARTH_INDEX[alpha+1][coord]] {term+=row[0]*(row[1]+row[2]*t).cos();}
            result[coord]+=powers[alpha]*term;alpha+=1;
        }
        result[coord]/=1e8;
    }
    result[0]=range(result[0],TAU);
    let l1=result[0]-radians(13.97*t-0.031*powers[2]);let c=l1.cos();let s=l1.sin();
    result[0]+=radians(-0.09033+0.03916*(c+s)*result[1].tan())/3600.0;
    result[1]+=radians(0.03916*(c-s))/3600.0;
    (range(result[0]-PI,TAU),-result[1],result[2])
}

fn cartesian(l:f64,b:f64,r:f64)->[f64;3]{let rb=r*b.cos();[rb*l.cos(),rb*l.sin(),r*b.sin()]}
fn spherical([x,y,z]:[f64;3])->(f64,f64,f64){let r=x*x+y*y;if r>0.0{(range(y.atan2(x),TAU),z.atan2(r.sqrt()),(r+z*z).sqrt())}else{(0.0,if z==0.0{0.0}else{z.signum()*PI/2.0},z.abs())}}

fn equatorial(mjd:f64,longitude:f64,latitude:f64,moon:bool)->(f64,f64) {
    let eps=obliquity(mjd);let se=eps.sin();let ce=eps.cos();
    let sy=latitude.sin();let mut cy=latitude.cos();if cy.abs()<1e-20{cy=1e-20;}
    let sx=longitude.sin();let cx=longitude.cos();
    let dec=(sy*ce+cy*se*sx).clamp(-1.0,1.0).asin();
    let ra=range(((sx*ce-(sy/cy)*se)/cx).atan()+if cx<0.0{PI}else{0.0},TAU);
    let (deps,dpsi)=nutation(mjd);let sp=dpsi.sin();let cp=dpsi.cos();let sede=(eps+deps).sin();let cede=(eps+deps).cos();
    let old=cartesian(ra,dec,1.0);
    let mut xyz=[cp*old[0]-sp*ce*old[1]-sp*se*old[2],
        cede*sp*old[0]+(cede*cp*ce+sede*se)*old[1]+(cede*cp*se-sede*ce)*old[2],
        sede*sp*old[0]+(sede*cp*ce-cede*se)*old[1]+(sede*cp*se+cede*ce)*old[2]];
    let (ra,dec,_)=spherical(xyz);
    if moon{return(ra,dec);}
    xyz=cartesian(ra,dec,1.0);
    let l=TAU*(0.27908+100.00214*(mjd-J2000)/36525.0);let c=l.cos();
    xyz[0]+=-0.994e-4*l.sin();xyz[1]+=0.912e-4*c;xyz[2]+=0.395e-4*c;
    let (ra,dec,_)=spherical(xyz);(ra,dec)
}

fn ecliptic_longitude(mjd:f64,ra:f64,dec:f64)->f64 {
    let eps=obliquity(mjd);let cosine=ra.cos();
    range(((ra.sin()*eps.cos()+dec.tan()*eps.sin())/cosine).atan()+if cosine<0.0{PI}else{0.0},TAU)
}

pub fn sidereal(time:f64,longitude:f64)->f64 {
    let mjd=time/86400.0+25567.5;let day=(mjd-0.5).floor()+0.5;let hour=(mjd-day)*24.0;
    let t=((day-0.5).trunc()+0.5-J2000)/36525.0;
    let base=range((24110.54841+(8640184.812866+(0.093104-6.2e-6*t)*t)*t)/3600.0,24.0);
    let mut lst=range(hour/0.9972695677+base,24.0)+longitude/15.0;
    let (deps,dpsi)=nutation(mjd);lst+=degrees(dpsi*(obliquity(mjd)+deps).cos())/15.0;
    radians(range(lst,24.0)*15.0)
}

pub fn unrefract(angle:f64,pressure:f64,temperature:f64)->f64 {
        let deg=degrees(angle);
        let low=||{let a=((2e-5*deg+1.96e-2)*deg+1.594e-1)*pressure;
            let b=(273.0+temperature)*((8.45e-2*deg+5.05e-1)*deg+1.0);let r=radians(a/b);
            if angle<0.0&&r<0.0{angle}else{angle-r}};
        let high=||angle-7.888888e-5*pressure/((273.0+temperature)*angle.tan());
        if deg<14.5{low()}else if deg>=15.5{high()}else{let lo=low();lo+(high()-lo)*(deg-14.5)}
}

pub fn refract(true_altitude:f64,pressure:f64,temperature:f64)->f64 {
    let mut previous=unrefract(true_altitude,pressure,temperature);let mut step=0.8*(true_altitude-previous);let mut apparent=true_altitude;
    for _ in 0..32 {
        apparent+=step;let next=unrefract(apparent,pressure,temperature);
        if (true_altitude-next).abs()<=radians(0.1/3600.0){return apparent;}
        step*=-(true_altitude-next)/(previous-next);previous=next;
    }
    apparent
}

pub fn position(body:&str,time:f64,observer:&Observer)->Result<Position> {
    validate(time,observer)?;
    let mjd=time/86400.0+25567.5;let dynamical=mjd+delta_t(time)/86400.0;
    let (sunlon,sunlat,sundistance)=solar_ecliptic(dynamical);
    let moon=body=="moon";ensure!(moon||body=="sun","body must be sun or moon");
    let (longitude,latitude,distance,_phase,illumination)=if moon {
        let (lon,lat,dist,ms,md)=lunar::coordinates(dynamical);
        let mut elong=(lat.cos()*(lon-sunlon).cos()).acos();
        if lon>sunlon+PI || lon>sunlon-PI && lon<sunlon {elong = -elong;}
        let correction=0.1468*elong.sin()*(1.0-0.0549*md.sin())/(1.0-0.0167*ms.sin());
        (lon,lat,dist,range(lon-sunlon,TAU),(1.0+(PI-elong-radians(correction)).cos())/2.0*100.0)
    }else{(sunlon,sunlat,sundistance,0.0,100.0)};
    let (ra,dec)=equatorial(dynamical,longitude,latitude,moon);
    let phase=if moon {
        let (sunra,sundec)=equatorial(dynamical,sunlon,sunlat,false);
        range(ecliptic_longitude(mjd,ra,dec)-ecliptic_longitude(mjd,sunra,sundec),TAU)
    }else{0.0};
    let ha=sidereal(time,observer.longitude)-ra;
    let phi=radians(observer.latitude);let cphi=phi.cos();let sphi=phi.sin();let e2=(2.0-1.0/298.257)/298.257;
    let robs=1.0/(1.0-e2*sphi*sphi).sqrt();let height=observer.elevation/EARTH_RADIUS;
    let xobs=(robs+height)*cphi;let zobs=(robs*(1.0-e2)+height)*sphi;
    let [x,y,z]=cartesian(-ha,dec,distance*AU/EARTH_RADIUS);
    let (lha,declination,radius)=spherical([x-xobs,y,z-zobs]);let hour_angle=range(-lha,TAU);
    let ca=declination.sin()*sphi+declination.cos()*cphi*hour_angle.cos();
    let altitude=ca.clamp(-1.0,1.0).asin();
    let sb=declination.cos();let c=declination.sin();let y=hour_angle.sin()*sb*cphi;
    let x=c-ca*sphi;
    let b=if cphi<1e-7{if sphi<0.0{hour_angle}else{PI-hour_angle}}else if y!=0.0{if x!=0.0{y.atan2(x)}else{if y>0.0{PI/2.0}else{-PI/2.0}}}else if x>=0.0{0.0}else{PI};
    let azimuth=range(TAU-range(b,TAU),TAU);
    let top_distance=radius*EARTH_RADIUS/AU;
    let angular_radius=if moon{(1.740e6/AU/top_distance).asin()}else{4.65242e-3/top_distance};
    Ok(Position {altitude:degrees(refract(altitude,observer.pressure,observer.temperature)),azimuth:degrees(azimuth),distance_au:top_distance,
        right_ascension:degrees(range(ra+ha-hour_angle,TAU)),declination:degrees(declination),hour_angle:degrees(hour_angle),
        geocentric_longitude:degrees(longitude),geocentric_latitude:degrees(latitude),angular_radius:degrees(angular_radius),
        illumination,phase_angle:degrees(phase),moon_fullness:if moon{lunar_fullness(mjd)}else{100.0},
        geocentric_ra:degrees(ra),geocentric_dec:degrees(dec)})
}

// WeeWX's moon_fullness uses PyEphem's separate colongitude model, while
// Moon.phase uses the ephemeris above. Preserve that distinction for skins.
#[allow(clippy::approx_constant)]
fn lunar_fullness(mjd:f64)->f64 {
    let rad=0.0174533;let t=(mjd-36525.0)/36525.0;let t2=t*t;let t3=t*t2;
    let l=280.466+36000.8*t;
    let m=(357.529+35999.0*t-0.0001536*t2+t3/24490000.0)*rad;
    let c=(1.915-0.004817*t-0.000014*t2)*m.sin()+(0.01999-0.000101*t)*(2.0*m).sin()+0.00029*(3.0*m).sin();
    let v=m+c*rad;let e=0.01671-0.00004204*t-0.0000001236*t2;
    let r=0.99972/(1.0+e*v.cos())*145980000.0;
    let lam0=l+c-0.00569-0.00478*((125.04-1934.1*t)*rad).sin();
    let f=(93.2721+483202.0*t-0.003403*t2-t3/3526000.0)*rad;
    let l1=(218.316+481268.0*t)*rad;
    let m1=(134.963+477199.0*t+0.008997*t2+t3/69700.0)*rad;
    let d2=(297.85+445267.0*t-0.00163*t2+t3/545900.0)*2.0*rad;
    let dr=385000.0-20954.0*m1.cos()-3699.0*(d2-m1).cos()-2956.0*d2.cos();
    let b=5.128*f.sin()+0.2806*(m1+f).sin()+0.2777*(m1-f).sin()+0.1732*(d2-f).sin();
    let sum=6.289*m1.sin()+1.274*(d2-m1).sin()+0.6583*d2.sin()+0.2136*(2.0*m1).sin()-0.1851*m.sin()-0.1143*(2.0*f).sin();
    let lam=l1+sum*rad;let cosine=(b*rad).cos()*(lam-lam0*rad).cos();
    let psi=cosine.clamp(-1.0,1.0).acos();let num=r*psi.sin();let den=dr-r*cosine;
    let mut angle=(num/den).atan();if num*den<0.0{angle+=3.14159;}
    if num<0.0{angle+=3.14159;}
    (1.0+angle.cos())*50.0
}
