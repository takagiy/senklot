use anyhow::{anyhow, Context, Result};
use daemonize::Daemonize;
use nom::character::complete::{digit0, digit1};
use nom::{alt, map, map_res, named, opt, recognize, tag, take, tuple};
use nom::combinator::all_consuming;
use serde::{Deserialize, Deserializer};
use std::collections::HashMap;
use std::fs;
use std::str::FromStr;
use std::thread;
use std::time::Duration;

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct Time {
    hours: u32,
    minutes: u32,
}

impl<'a> Deserialize<'a> for Time {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where D: Deserializer<'a> {
        use serde::de::Error;
        let string = Deserialize::deserialize(deserializer)?;
        let (_, o) = all_consuming(time)(string).map_err(Error::custom)?;
        Ok(o)
    }
}

struct StaticDuration {
    begin: Time,
    end: Time,
}

impl<'a> Deserialize<'a> for StaticDuration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where D: Deserializer<'a> {
        use serde::de::Error;
        let string = Deserialize::deserialize(deserializer)?;
        let (_, o) = all_consuming(static_duration)(string).map_err(Error::custom)?;
        Ok(o)
    }
}

enum DurationUnit {
    Minutes,
    Hours,
}

struct DynamicDuration {
    duration: f64,
    unit: DurationUnit,
}

impl<'a> Deserialize<'a> for DynamicDuration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where D: Deserializer<'a> {
        use serde::de::Error;
        let string = Deserialize::deserialize(deserializer)?;
        let (_, o) = all_consuming(dybamic_duration)(string).map_err(Error::custom)?;
        Ok(o)
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Restriction {
    Static { unlock: Vec<StaticDuration> },
    Dynamic { period: DynamicDuration, cool_time: DynamicDuration },
}

#[derive(Deserialize)]
struct Entry {
    domains: Vec<String>,
    #[serde(flatten)]
    restriction: Restriction,
}

#[derive(Deserialize)]
struct Config {
    #[serde(deserialize_with = "deserialize_secs", default = "default_interval")]
    interval: Duration,
    #[serde(flatten)]
    entries: HashMap<String, Entry>,
}

fn deserialize_secs<'a, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: Deserializer<'a>,
{
    let seconds = Deserialize::deserialize(deserializer)?;
    Ok(Duration::from_secs(seconds))
}

fn default_interval() -> Duration {
    Duration::from_secs(60)
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

        Ok(Time{hours: h, minutes: m})
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
    alt!
    ( map!(tag!("h"), |_| DurationUnit::Hours)
    | map!(tag!("m") ,|_| DurationUnit::Minutes)
    )
);
named!(dybamic_duration(&str) -> DynamicDuration,
    map!(tuple!(float, unit), |(d, u)| {
        DynamicDuration { duration: d, unit: u }
    })
);

fn read_config_file() -> Result<String> {
    let xdg_dirs = xdg::BaseDirectories::with_prefix("senklot")?;
    let config_file = xdg_dirs.find_config_file("config").ok_or(anyhow!(""))?;
    let content = fs::read_to_string(config_file)?;
    Ok(content)
}

fn parse_config(config: &str) -> Result<Config> {
    let config = toml::from_str(config)?;
    Ok(config)
}

fn daemonize() -> Result<()> {
    let _ = Daemonize::new().pid_file("/tmp/senklot.pid").start()?;
    Ok(())
}

fn main() -> Result<()> {
    let config = read_config_file()?;
    let config = parse_config(&config)?;
    daemonize()?;
    loop {
        thread::sleep(config.interval);
    }
}
