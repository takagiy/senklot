use anyhow::{anyhow, Context, Result};
use chrono::offset::Local;
use chrono::{DateTime, Duration, Timelike};
use crossbeam::channel;
use crossbeam::channel::{select, tick};
use daemonize::Daemonize;
use nom::character::complete::{digit0, digit1, none_of, space0, space1};
use nom::combinator::all_consuming;
use nom::{alt, many1, map, map_res, named, opt, recognize, tag, take, tuple};
use notify::event::*;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Deserializer, Serialize};
use std::borrow::ToOwned;
use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::path::Path;
use std::process;
use std::str::FromStr;

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

#[derive(Deserialize, Serialize)]
struct State {
    last_unlocked: HashMap<String, LocalTime>,
    last_locked: HashMap<String, LocalTime>,
    is_locked: HashMap<String, bool>,
    #[serde(skip)]
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
    fn load_with(config: &Config, state_file: Option<Vec<u8>>) -> State {
        let mut domain_map = HashMap::new();
        for (name, entry) in &config.entries {
            for domain in &entry.domains {
                domain_map.insert(domain.clone(), name.clone());
            }
        }

        let state = match state_file {
            Some(state_file) => bincode::deserialize(&state_file).unwrap_or(State::empty()),
            None => State::empty(),
        };

        State {
            domain_map: domain_map,
            ..state
        }
    }

    fn empty() -> State {
        State {
            domain_map: HashMap::new(),
            last_unlocked: HashMap::new(),
            last_locked: HashMap::new(),
            is_locked: HashMap::new(),
        }
    }

    fn export(&self) -> Vec<u8> {
        bincode::serialize(&self).unwrap()
    }

    fn unlock(&mut self, name: &str, entry: &Entry, after_unlock: &Option<String>) -> Result<()> {
        if !self.is_locked.get(name).unwrap_or(&true) {
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

        println!("{}", self.is_locked[name]);

        self.commit()?;

        if let Some(cmd) = after_unlock {
            process::Command::new("sh").arg("-c").arg(cmd).spawn()?;
        }

        Ok(())
    }

    fn lock(&mut self, name: &str, entry: &Entry, after_lock: &Option<String>) -> Result<()> {
        if *self.is_locked.get(name).unwrap_or(&false) {
            return Ok(());
        }

        self.is_locked.set(name, true);
        if let Restriction::Dynamic { .. } = entry.restriction {
            self.last_locked.set(name, Local::now());
        }

        let _ = self.commit();

        if let Some(cmd) = after_lock {
            process::Command::new("sh").arg("-c").arg(cmd).spawn()?;
        }

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

    fn update(&mut self, config: &Config) {
        let now: LocalTime = Local::now();
        for (name, entry) in &config.entries {
            match &entry.restriction {
                Restriction::Static { unlock } => {
                    if unlock.iter().any(|d| d.contains(&now)) {
                        if let Err(e) = self.unlock(&name, &entry, &config.after_unlock) {
                            println!("{:?}", e);
                        }
                    } else {
                        if let Err(e) = self.lock(&name, &entry, &config.after_lock) {
                            println!("{:?}", e);
                        }
                    }
                }
                Restriction::Dynamic { period, .. } => {
                    if self
                        .last_unlocked
                        .get(name)
                        .map(|last_unlocked| now < *last_unlocked + period.clone())
                        .unwrap_or(true)
                    {
                        let _ = self.unlock(&name, &entry, &config.after_unlock);
                    } else {
                        let _ = self.lock(&name, &entry, &config.after_lock);
                    }
                }
            }
        }
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
    //let xdg_dirs = xdg::BaseDirectories::with_prefix("senklot")?;
    //let config_file = xdg_dirs.find_config_file("config").ok_or(anyhow!("Could not find config directory"))?;
    let config_file = "/home/takagiy/.config/senklot/config";
    let content = fs::read_to_string(config_file)?;
    Ok(content)
}

fn read_state_file() -> Result<Option<Vec<u8>>> {
    let path = Path::new("/var/lib/senklot");
    if path.is_file() {
        let content = fs::read(path)?;
        Ok(Some(content))
    } else {
        Ok(None)
    }
}

fn parse_config(config: &str) -> Result<Config> {
    let config = toml::from_str(config)?;
    Ok(config)
}

fn open_file<P: AsRef<Path>>(path: P) -> Result<File> {
    let file = File::create(path.as_ref()).or_else(|_| File::open(path))?;
    Ok(file)
}

fn daemonize() -> Result<()> {
    let stdout = open_file("/tmp/senklot.log").with_context(|| "cannot open stdout")?;
    let stderr = open_file("/tmp/senklot.err").with_context(|| "cannot open stderr")?;
    let _ = Daemonize::new()
        .stdout(stdout)
        .stderr(stderr)
        .user(0)
        .chown_pid_file(true)
        .pid_file("/tmp/senklot.pid")
        .start()
        .context("Failed to start daemon")?;
    Ok(())
}

fn exit_channel() -> Result<channel::Receiver<()>> {
    let (tx, rx) = channel::bounded(0);
    ctrlc::set_handler(move || {
        let _ = tx.send(());
    })?;

    Ok(rx)
}

fn hosts_modified_channel() -> Result<(RecommendedWatcher, channel::Receiver<()>)> {
    let (tx, rx) = channel::bounded(0);
    let mut watcher: RecommendedWatcher = Watcher::new_immediate(move |event| {
        if let Ok(Event {
            kind: EventKind::Modify(ModifyKind::Data(_)),
            ..
        }) = event
        {
            let _ = tx.send(());
        }
    })?;
    watcher.watch("/etc/hosts", RecursiveMode::NonRecursive)?;

    Ok((watcher, rx))
}

fn main_loop(config: Config, mut state: State) -> Result<()> {
    let ticker = tick(config.interval.to_std().unwrap());
    let exit = exit_channel()?;
    let (_watcher, hosts_modified) = hosts_modified_channel()?;
    loop {
        select! {
            recv(ticker) -> _ => {
                state.update(&config);
            },
            recv(exit) -> _ => {
                if let Err(e) = fs::write("/var/lib/senklot", state.export()) {
                    println!("{:?}", e);
                }
                process::exit(0);
            },
            recv(hosts_modified) -> _ => {
                if let Err(e) = state.commit() {
                    println!("{:?}", e);
                }
            }
        }
    }
}

fn main() -> Result<()> {
    let config = read_config_file().context("Unable to read config")?;
    let config = parse_config(&config).context("Parse error in config")?;
    let state = read_state_file().context("Unable to read state file")?;
    let state = State::load_with(&config, state);

    daemonize()?;
    main_loop(config, state)?;

    Ok(())
}
