use anyhow::{anyhow, Result};
use chrono::offset::Local;
use chrono::{DateTime, Duration, NaiveTime as Time};
use nom::character::complete::{digit0, digit1};
use nom::combinator::all_consuming;
use nom::{alt, map, map_res, named, opt, recognize, tag, take, tuple};
use serde::{Deserialize, Deserializer};
use std::collections::HashMap;
use std::str::FromStr;

pub type LocalTime = DateTime<Local>;

pub struct StaticDuration {
    pub begin: Time,
    pub end: Time,
}

impl StaticDuration {
    pub fn contains(&self, time: &LocalTime) -> bool {
        let t = time.time();

        if self.begin < self.end {
            self.begin <= t && t < self.end
        } else {
            self.end <= t || t < self.begin
        }
    }
}

impl<'a> Deserialize<'a> for StaticDuration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'a>,
    {
        use serde::de::Error;
        let string = Deserialize::deserialize(deserializer)?;
        let (_, o) = all_consuming(static_duration)(string).map_err(Error::custom)?;
        Ok(o)
    }
}

pub enum DurationUnit {
    Minutes,
    Hours,
}

fn deserialize_hm<'a, D>(deserializer: D) -> Result<chrono::Duration, D::Error>
where
    D: Deserializer<'a>,
{
    use serde::de::Error;
    let string = Deserialize::deserialize(deserializer)?;
    let (_, o) = all_consuming(mh_duration)(string).map_err(Error::custom)?;
    Ok(o.into())
}

#[derive(Deserialize)]
#[serde(untagged)]
pub enum Restriction {
    Static {
        unlock: Vec<StaticDuration>,
    },
    Dynamic {
        #[serde(deserialize_with = "deserialize_hm")]
        period: chrono::Duration,
        #[serde(deserialize_with = "deserialize_hm")]
        cool_time: chrono::Duration,
    },
}

#[derive(Deserialize)]
pub struct Entry {
    pub domains: Vec<String>,
    #[serde(flatten)]
    pub restriction: Restriction,
}

#[derive(Deserialize)]
pub struct Config {
    pub after_lock: Option<String>,
    pub after_unlock: Option<String>,
    #[serde(deserialize_with = "deserialize_secs", default = "default_interval")]
    pub interval: Duration,
    #[serde(flatten)]
    pub entries: HashMap<String, Entry>,
}

pub fn deserialize_secs<'a, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: Deserializer<'a>,
{
    let seconds = Deserialize::deserialize(deserializer)?;
    Ok(Duration::seconds(seconds))
}

pub fn default_interval() -> Duration {
    Duration::seconds(60)
}

named!(two_digits(&str) -> u32, map_res!(take!(2), u32::from_str));
named!(time(&str) -> Time,
    map_res!(tuple!(two_digits, tag!(":"), two_digits), |(h, _, m)| {
        if h > 23  {
            return Err(anyhow!("Invalid hours"));
        }
        if m > 60  {
            return Err(anyhow!("Invalid minutes"));
        }

        Ok(Time::from_hms(h, m, 0))
    })
);
named!(static_duration(&str) -> StaticDuration,
    map!(tuple!(time, tag!("-"), time), |(b, _, e)| {
        StaticDuration{begin: b, end: e}
    })
);
named!(float(&str) -> f64,
    map_res!(recognize!(tuple!(digit1, opt!(tuple!(tag!("."), digit0)))), f64::from_str)
);
named!(unit(&str) -> DurationUnit,
    alt!( map!(tag!("h"), |_| DurationUnit::Hours)
        | map!(tag!("m") ,|_| DurationUnit::Minutes)
        )
);
named!(mh_duration(&str) -> Duration,
    map!(tuple!(float, unit), |(d, u)| {
        match u {
            DurationUnit::Minutes => Duration::minutes(d.trunc() as i64),
            DurationUnit::Hours   => Duration::hours(d.trunc() as i64) + Duration::minutes((d.fract() / 60.).trunc() as i64)
        }
    })
);
