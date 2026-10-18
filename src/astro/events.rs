use super::*;

#[derive(Debug,Serialize)]
pub struct Events {
    pub rises: Vec<f64>, pub sets: Vec<f64>, pub transits: Vec<f64>, pub antitransits: Vec<f64>,
    pub always_up: bool, pub never_up: bool,
}

fn signed(degrees:f64)->f64{range(degrees+180.0,360.0)-180.0}

fn root(mut start:f64,mut stop:f64,mut first:f64,mut function:impl FnMut(f64)->Result<f64>)->Result<f64>{
    for _ in 0..48 {
        let middle=(start+stop)/2.0;let value=function(middle)?;
        if stop-start<0.001{return Ok(middle);}
        if (first<0.0)==(value<0.0){start=middle;first=value;}else{stop=middle;}
    }
    Ok((start+stop)/2.0)
}

pub fn events(body:&str,start:f64,stop:f64,observer:&Observer,center:bool)->Result<Events>{
    validate(start,observer)?;validate(stop,observer)?;
    ensure!(start<=stop&&stop-start<=370.0*86400.0,"event range must cover at most 370 days");
    if let Some(result)=iterative_events(body,start,stop,observer,center)? {return Ok(result);}
    sampled_events(body,start,stop,observer,center)
}

fn iterative_events(body:&str,start:f64,stop:f64,observer:&Observer,center:bool)->Result<Option<Events>>{
    let mut geometric=observer.clone();geometric.pressure=0.0;
    let mut result=Events{rises:vec![],sets:vec![],transits:vec![],antitransits:vec![],always_up:false,never_up:false};
    for kind in 0..4 {
        let output=match kind{0=>&mut result.rises,1=>&mut result.sets,2=>&mut result.transits,_=>&mut result.antitransits};
        let mut search=start;
        while search<stop {
            let mut stamp=search;let mut found=false;
            for iteration in 0..32 {
                let p=position(body,stamp,&geometric)?;
                let target=if kind<2 {
                    let radius=if center{0.0}else{((p.angular_radius*7200.0)as f32)as f64/7200.0};
                    let horizon=unrefract(radians(observer.horizon-radius),observer.pressure,observer.temperature);
                    let latitude=radians(observer.latitude);let declination=radians(p.declination);
                    let arg=(horizon.sin()-latitude.sin()*declination.sin())/(latitude.cos()*declination.cos());
                    // Near a grazing horizon, fixed point iteration can miss
                    // a short visibility interval. Use the bracketed search.
                    if arg.abs()>0.999{return Ok(None);}
                    degrees(arg.acos())*if kind==0{-1.0}else{1.0}
                }else if kind==2{0.0}else{180.0};
                let difference=if iteration==0 {range(target-p.hour_angle,360.0)}else{signed(target-p.hour_angle)};
                let bump=difference/360.0*86400.0;
                if bump.abs()<0.0005 {found=true;break;}
                stamp+=bump;
                if stamp<search-3600.0||stamp>stop+2.0*86400.0{return Ok(None);}
            }
            if !found{return Ok(None);}
            if stamp>=stop{break;}
            if stamp>start{output.push(stamp);}
            search=stamp+0.01;
        }
    }
    if result.rises.is_empty()&&result.sets.is_empty(){return Ok(None);}
    Ok(Some(result))
}

fn sampled_events(body:&str,start:f64,stop:f64,observer:&Observer,center:bool)->Result<Events>{
    let mut geometric=observer.clone();geometric.pressure=0.0;
    let height=|time|->Result<f64>{
        let p=position(body,time,&geometric)?;
        let radius=if center{0.0}else{((p.angular_radius*7200.0)as f32)as f64/7200.0};
        let horizon=unrefract(radians(observer.horizon-radius),observer.pressure,observer.temperature);
        Ok(p.altitude-degrees(horizon))
    };
    let mut result=Events {rises:vec![],sets:vec![],transits:vec![],antitransits:vec![],always_up:true,never_up:true};
    let mut stamp=start;let mut previous=height(start)?;let mut hour=position(body,start,observer)?.hour_angle;
    let mut earlier:Option<(f64,f64)>=None;
    result.always_up &= previous>=0.0;result.never_up &= previous<0.0;
    while stamp<stop {
        let next=(stamp+600.0).min(stop);let value=height(next)?;let next_hour=position(body,next,observer)?.hour_angle;
        result.always_up &= value>=0.0;result.never_up &= value<0.0;
        if previous<0.0&&value>=0.0 {result.rises.push(root(stamp,next,previous,height)?);}
        else if previous>=0.0&&value<0.0 {result.sets.push(root(stamp,next,previous,height)?);}
        if let Some((first_time,first_value))=earlier {
            let maximum=previous>first_value&&previous>value;
            let minimum=previous<first_value&&previous<value;
            if (maximum||minimum)&&previous.abs()<1.0&&(first_value<0.0)==(previous<0.0)&&(value<0.0)==(previous<0.0) {
                let (mut left,mut right)=(first_time,next);
                for _ in 0..32 {
                    let a=left+(right-left)/3.0;let b=right-(right-left)/3.0;
                    if (height(a)?<height(b)?)==maximum{left=a;}else{right=b;}
                }
                let peak=(left+right)/2.0;let peak_value=height(peak)?;
                result.always_up &= peak_value>=0.0;result.never_up &= peak_value<0.0;
                if (peak_value<0.0)!=(previous<0.0) {
                    let a=root(first_time,peak,first_value,height)?;let b=root(peak,next,peak_value,height)?;
                    if maximum{result.rises.push(a);result.sets.push(b);}else{result.sets.push(a);result.rises.push(b);}
                }
            }
        }
        if signed(hour)<0.0&&signed(next_hour)>=0.0&&signed(next_hour)-signed(hour)<180.0 {
            result.transits.push(root(stamp,next,signed(hour),|t|Ok(signed(position(body,t,observer)?.hour_angle)))?);
        }
        if hour<180.0&&next_hour>=180.0 {
            result.antitransits.push(root(stamp,next,hour-180.0,|t|Ok(position(body,t,observer)?.hour_angle-180.0))?);
        }
        earlier=Some((stamp,previous));stamp=next;previous=value;hour=next_hour;
    }
    Ok(result)
}

pub fn phase_events(start:f64,stop:f64)->Result<Vec<(String,f64)>>{
    ensure!(start.is_finite()&&stop.is_finite()&&start<=stop&&stop-start<=370.0*86400.0,"invalid lunar phase range");
    let observer=Observer{latitude:0.0,longitude:0.0,elevation:0.0,pressure:0.0,temperature:15.0,horizon:0.0};
    let mut result=vec![];
    for (name,target) in [("new_moon",0.0),("first_quarter",90.0),("full_moon",180.0),("last_quarter",270.0)] {
        let angle=|time|->Result<f64>{Ok(signed(position("moon",time,&observer)?.phase_angle-target))};
        let mut stamp=start;let mut previous=angle(start)?;
        while stamp<stop {
            let next=(stamp+43200.0).min(stop);let value=angle(next)?;
            if previous<0.0&&value>=0.0&&value-previous<180.0 {result.push((String::from(name),root(stamp,next,previous,angle)?));}
            stamp=next;previous=value;
        }
    }
    result.sort_by(|a,b|a.1.total_cmp(&b.1));Ok(result)
}
