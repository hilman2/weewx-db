use super::{data::*,lunar_expressions,range,TAU};

fn series(jed:f64,args:&[i32],tables:&[&[f64]],harmonics:&[usize;18],elements:&[f64;18])->Vec<f64> {
    let t=(jed-2451545.0)/3652500.0;let mut ss=[[0.0;30];18];let mut cc=[[0.0;30];18];
    for k in 0..18 {
        if harmonics[k]==0{continue;}
        let angle=4.848_136_811_095_36e-6*elements[k];let su=angle.sin();let cu=angle.cos();
        ss[k][0]=su;cc[k][0]=cu;let mut sv=2.0*su*cu;let mut cv=cu*cu-su*su;
        ss[k][1]=sv;cc[k][1]=cv;
        for i in 2..harmonics[k]{let s=su*cv+cu*sv;cv=cu*cv-su*sv;sv=s;ss[k][i]=sv;cc[k][i]=cv;}
    }
    let mut indexes=vec![0;tables.len()];let mut sums=vec![0.0;tables.len()];let mut offset=0;
    loop {
        let count=args[offset];offset+=1;if count<0{break;}
        if count==0{
            let power=args[offset] as usize;offset+=1;
            for (i,table) in tables.iter().enumerate(){let mut value=table[indexes[i]];indexes[i]+=1;
                for _ in 0..power{value=value*t+table[indexes[i]];indexes[i]+=1;}sums[i]+=value;}
            continue;
        }
        let(mut sv,mut cv)=(0.0,0.0);let mut first=true;
        for _ in 0..count {
            let harmonic=args[offset];let planet=(args[offset+1]-1) as usize;offset+=2;
            if harmonic==0{continue;}
            let k=harmonic.unsigned_abs() as usize-1;let su=ss[planet][k]*if harmonic<0{-1.0}else{1.0};let cu=cc[planet][k];
            if first{sv=su;cv=cu;first=false;}else{let value=su*cv+cu*sv;cv=cu*cv-su*sv;sv=value;}
        }
        let power=args[offset] as usize;offset+=1;
        for(i,table)in tables.iter().enumerate(){let(mut cu,mut su)=(table[indexes[i]],table[indexes[i]+1]);indexes[i]+=2;
            for _ in 0..power{cu=cu*t+table[indexes[i]];su=su*t+table[indexes[i]+1];indexes[i]+=2;}
            sums[i]+=cu*cv+su*sv;}
    }
    for value in &mut sums{*value*=1e-4;}sums
}

pub fn coordinates(mjd:f64)->(f64,f64,f64,f64,f64) {
    let approximate=lunar_expressions::moon_fast(mjd);
    let distance=6378.16/1.4959787e8/approximate[2].sin();
    let jed=mjd+2415020.0-distance*5.7755183e-3;
    let(elements,lp)=lunar_expressions::mean_elements(jed);
    let lr=series(jed,&LUNAR_ARGUMENTS,&[&LUNAR_LONGITUDE,&LUNAR_RADIUS],&[3,26,29,23,5,10,0,0,0,8,4,4,6,2,0,0,0,0],&elements);
    let b=series(jed,&LATITUDE_ARGUMENTS,&[&LUNAR_LATITUDE],&[0,26,29,8,3,5,0,0,0,6,5,3,5,1,0,0,0,0],&elements)[0];
    let mut longitude=lr[0]+lp;if longitude < -6.45e5{longitude+=1.296e6;}
    if longitude>6.45e5{longitude-=1.296e6;}
    let scale=4.848_136_811_095_36e-6;
    (range(scale*longitude,TAU),scale*b,(scale*lr[1]+1.0)*2.57356868953e-3,scale*elements[11],scale*elements[12])
}
