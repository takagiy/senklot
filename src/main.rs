use anyhow::{anyhow, Context, Result};
use chrono::offset::Local;
use chrono::{DateTime, Duration, Timelike};
use daemonize::Daemonize;
use nom::character::complete::{none_of, digit0, digit1, space0, space1};
use nom::combinator::all_consuming;
use nom::{alt, many1, map, map_res, named, none_of, opt, recognize, tag, take, tuple};
use serde::{Deserialize, Deserializer};
use std::borrow::ToOwned;
use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::str::FromStr;
use std::thread;

type LocalTime = DateTime<Local>;

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct Time {
    hours: u32,
    minutes: u32,
}

impl<'a> Deserialize<'a> for Time {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'a>,
    {
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

impl StaticDuration {
    fn contains(&self, time: &LocalTime) -> bool {
        let t = Time {
            hours: time.hour(),
            minutes: time.minute(),
        };
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

enum DurationUnit {
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
enum Restriction {
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
struct Entry {
    domains: Vec<String>,
    #[serde(flatten)]
    restriction: Restriction,
}

#[derive(Deserialize)]
struct Config {
    after_lock: Option<String>,
    after_unlock: Option<String>,
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
    Ok(Duration::seconds(seconds))
}

fn default_interval() -> Duration {
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
named!(mh_duration(&str) -> Duration,
    map!(tuple!(float, unit), |(d, u)| {
        match u {
            DurationUnit::Minutes => Duration::minutes(d.trunc() as i64),
            DurationUnit::Hours   => Duration::hours(d.trunc() as i64) + Duration::minutes((d.fract() / 60.).trunc() as i64)
        }
    })
);
named!(addr_domain(&str) -> String,
    map!(recognize!(many1!(none_of("\t #"))), |s| s.to_owned())
);
named!(comment_out(&str) -> (String, Host),
    map!(tuple!(space0, tag!("#"), locked_host), |(_, _, (domain, _))| (domain, Host::CommentedOut))
);
named!(locked_host(&str) -> (String, Host),
    map!(tuple!(space0, addr_domain, space1, addr_domain), |(_, _, _, domain)| (domain, Host::Locked))
);
named!(host(&str) -> (String, Host),
    alt!( locked_host
        | comment_out
        )
);

struct State {
    last_unlocked: HashMap<String, LocalTime>,
    last_locked: HashMap<String, LocalTime>,
    is_locked: HashMap<String, bool>,
    domain_map: HashMap<String, String>,
}

trait MutDict<V> {
    fn set(&mut self, key: &str, value: V);
}

impl<V> MutDict<V> for HashMap<String, V> {
    fn set(&mut self, key: &str, value: V) {
        match self.get_mut(key) {
            Some(dist) => {
                *dist = value;
            }
            None => {
                self.insert(key.to_owned(), value);
            }
        }
    }
}

impl State {
    fn unlock(&mut self, name: &str, entry: &Entry) -> Result<()> {
        if !self.is_locked.get(name).unwrap_or(&false) {
            return Ok(());
        }
        if let Restriction::Dynamic { cool_time, .. } = entry.restriction {
            if let Some(last_unlocked) = self.last_unlocked.get(name) {
                if Local::now() < *last_unlocked + cool_time.clone() {
                    return Err(anyhow!("Not have been cool down yet"));
                }
            }
        }

        self.is_locked.set(name, false);
        if let Restriction::Dynamic { .. } = entry.restriction {
            self.last_unlocked.set(name, Local::now());
        }

        let _ = self.commit();

        Ok(())
    }

    fn lock(&mut self, name: &str, entry: &Entry) -> Result<()> {
        if *self.is_locked.get(name).unwrap_or(&false) {
            return Ok(());
        }

        self.is_locked.set(name, true);
        if let Restriction::Dynamic { .. } = entry.restriction {
            self.last_locked.set(name, Local::now());
        }

        let _ = self.commit();

        Ok(())
    }

    fn domanin_is_locked(&self, domain: &str) -> bool {
        self.domain_map
            .get(domain)
            .and_then(|entry| self.is_locked.get(entry).cloned())
            .unwrap_or(false)
    }

    fn commit(&self) -> Result<()> {
        let hosts_file = read_hosts()?;
        let mut hosts = Hosts::parse(hosts_file);

        let mut state_changed = false;
        for domain in self.domain_map.keys() {
            if hosts.is_locked(domain) == self.domanin_is_locked(domain) {
                continue;
            }
            state_changed = true;
            hosts.write_state(domain, self.domanin_is_locked(domain));
        }

        if !state_changed {
            return Ok(());
        }

        let hosts_file = hosts.export();
        write_hosts(&hosts_file)?;

        Ok(())
    }
}

enum Host {
    Locked,
    None,
    CommentedOut,
}

struct Hosts {
    hosts_file: Vec<String>,
    hosts: HashMap<String, (usize, Host)>,
}

impl Hosts {
    fn parse(hosts_file: String) -> Hosts {
        let mut hosts = HashMap::new();
        for (line_number, line) in hosts_file.lines().enumerate() {
            if let Ok((_, (domain, host))) = host(line) {
                hosts.insert(domain, (line_number, host));
            }
        }

        Hosts {
            hosts_file: hosts_file.lines().map(ToOwned::to_owned).collect(),
            hosts: hosts,
        }
    }

    fn is_locked(&self, domain: &str) -> bool {
        match self
            .hosts
            .get(domain)
            .map(|(_, h)| h)
            .unwrap_or(&Host::None)
        {
            Host::Locked => true,
            Host::CommentedOut | Host::None => false,
        }
    }

    fn host_line(&self, domain: &str, is_locked: bool) -> String {
        if is_locked {
            format!("127.0.0.1 {}", domain)
        } else {
            format!("# 127.0.0.1 {}", domain)
        }
    }

    fn write_state(&mut self, domain: &str, is_locked: bool) {
        match self.hosts.get(domain).as_deref() {
            Some((line_number, _)) => {
                self.hosts_file[*line_number] = self.host_line(domain, is_locked)
            }
            None => self.hosts_file.push(self.host_line(domain, is_locked)),
        }
    }

    fn export(self) -> String {
        self.hosts_file.join("\n")
    }
}

fn write_hosts(content: &str) -> Result<()> {
    let _ = fs::write("/etc/hosts", content)?;
    Ok(())
}

fn read_hosts() -> Result<String> {
    let content = fs::read_to_string("/etc/hosts")?;
    Ok(content)
}

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
    let stdout = File::create("/tmp/senklot.log")?;
    let _ = Daemonize::new()
        .stdout(stdout)
        .pid_file("/tmp/senklot.pid")
        .start()?;
    Ok(())
}

fn main() -> Result<()> {
    let config = read_config_file()?;
    let config = parse_config(&config)?;
    let mut state = State {
        last_unlocked: HashMap::new(),
        last_locked: HashMap::new(),
        is_locked: HashMap::new(),
        domain_map: HashMap::new(),
    };

    // For debug
    let hosts = Hosts::parse(read_hosts()?);
    for domain in hosts.hosts.keys() {
        println!("{}", domain);
    }
    println!("{}", hosts.export());
    let (i, d) = addr_domain("abc#")?;
    println!("{}", d);

    daemonize()?;
    loop {
        let now: LocalTime = Local::now();
        for (name, entry) in &config.entries {
            match &entry.restriction {
                Restriction::Static { unlock } => {
                    if unlock.iter().any(|d| d.contains(&now)) {
                        let _ = state.unlock(&name, &entry);
                    } else {
                        let _ = state.lock(&name, &entry);
                    }
                }
                Restriction::Dynamic { period, .. } => {
                    if state
                        .last_unlocked
                        .get(name)
                        .map(|last_unlocked| now < *last_unlocked + period.clone())
                        .unwrap_or(true)
                    {
                        let _ = state.unlock(&name, &entry);
                    } else {
                        let _ = state.lock(&name, &entry);
                    }
                }
            }
        }
        thread::sleep(config.interval.to_std().unwrap());
    }
}
