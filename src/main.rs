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
use std::io::prelude::*;
use std::os::unix::net;
use std::path::{Path, PathBuf};
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
        if self.is_locked.get(name).and_if(|is_locked| !is_locked) {
            return Ok(());
        }
        if let Restriction::Dynamic { cool_time, .. } = entry.restriction {
            if self
                .last_unlocked
                .get(name)
                .and_if(|last_unlocked| Local::now() < *last_unlocked + cool_time.clone())
            {
                return Err(anyhow!("Not have been cool down yet"));
            }
        }

        self.is_locked.set(name, false);

        if matches!(entry.restriction, Restriction::Dynamic{..}) {
            self.last_unlocked.set(name, Local::now());
        }

        self.commit()?;

        if let Some(cmd) = after_unlock {
            process::Command::new("sh").arg("-c").arg(cmd).spawn()?;
        }

        Ok(())
    }

    fn lock(&mut self, name: &str, entry: &Entry, after_lock: &Option<String>) -> Result<()> {
        if self.is_locked.get(name).and_if(|is_locked| *is_locked) {
            return Ok(());
        }

        self.is_locked.set(name, true);

        if matches!(entry.restriction, Restriction::Dynamic{..}) {
            self.last_locked.set(name, Local::now());
        }

        self.commit()?;

        if let Some(cmd) = after_lock {
            process::Command::new("sh").arg("-c").arg(cmd).spawn()?;
        }

        Ok(())
    }

    fn domanin_is_locked(&self, domain: &str) -> bool {
        self.domain_map
            .get(domain)
            .and_if_flat(|entry| self.is_locked.get(entry).cloned())
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

    fn update(&mut self, config: &Config) -> Result<(), Vec<anyhow::Error>> {
        let mut errors = Vec::new();

        let now: LocalTime = Local::now();
        for (name, entry) in &config.entries {
            match &entry.restriction {
                Restriction::Static { unlock } => {
                    if unlock.iter().any(|d| d.contains(&now)) {
                        self.unlock(&name, &entry, &config.after_unlock)
                            .err()
                            .map(|e| {
                                errors.push(e);
                            });
                    } else {
                        self.lock(&name, &entry, &config.after_lock).err().map(|e| {
                            errors.push(e);
                        });
                    }
                }
                Restriction::Dynamic { period, .. } => {
                    if self
                        .last_unlocked
                        .get(name)
                        .or_if(|last_unlocked| now < *last_unlocked + period.clone())
                    {
                        self.unlock(&name, &entry, &config.after_unlock)
                            .err()
                            .map(|e| {
                                errors.push(e);
                            });
                    } else {
                        self.lock(&name, &entry, &config.after_lock).err().map(|e| {
                            errors.push(e);
                        });
                    }
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

enum Host {
    Locked,
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
        match self.hosts.get(domain) {
            None => false,
            Some((_, host)) => match host {
                Host::CommentedOut => false,
                Host::Locked => true,
            },
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
    let config_file = "/etc/senklot/config";
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

fn daemonize() -> Result<()> {
    fs::create_dir_all("/tmp/senklot")?;
    let stdout = File::create("/tmp/senklot/stdout.log")
        .context("Unable to open /tmp/senklot/stdout.log")?;
    let stderr = File::create("/tmp/senklot/stderr.log")
        .context("Unable to open /tmp/senklot/stderr.log")?;
    let _ = Daemonize::new()
        .stdout(stdout)
        .stderr(stderr)
        .pid_file("/tmp/senklot/senklot.pid")
        .start()
        .context("Unable to start daemon")?;
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

fn unlock_request_channel() -> Result<(SocketPath, channel::Receiver<String>)> {
    let (tx, rx) = channel::bounded(0);
    let (path, listener) = SocketPath::bind("/var/lib/senklot.socket")?;
    path.allow_write()?;
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if let Ok(mut stream) = stream {
                let mut buffer = String::new();
                if stream.read_to_string(&mut buffer).is_ok() {
                    let _ = tx.send(buffer);
                };
            }
        }
    });
    Ok((path, rx))
}

struct SocketPath {
    path: PathBuf,
}

impl SocketPath {
    fn bind<P: AsRef<Path>>(path: P) -> Result<(SocketPath, net::UnixListener)> {
        let listener = net::UnixListener::bind(path.as_ref())?;
        Ok((
            SocketPath {
                path: path.as_ref().to_path_buf(),
            },
            listener,
        ))
    }

    fn allow_write(&self) -> Result<()> {
        let mut permissions = fs::metadata(&self.path)?.permissions();
        permissions.set_readonly(false);
        let ok = fs::set_permissions(&self.path, permissions)?;
        Ok(ok)
    }
}

impl Drop for SocketPath {
    fn drop(&mut self) {
        fs::remove_file(&self.path).expect("Unable to remove the socket");
    }
}

trait Optional<T> {
    fn into_option(self) -> Option<T>;
}

trait OptionCond<T> {
    fn and_if_flat<F: FnOnce(T) -> Option<bool>>(self, pred: F) -> bool;
    fn or_if_flat<F: FnOnce(T) -> Option<bool>>(self, pred: F) -> bool;
    fn and_if<F: FnOnce(T) -> bool>(self, pred: F) -> bool;
    fn or_if<F: FnOnce(T) -> bool>(self, pred: F) -> bool;
}

impl<T, O: Optional<T>> OptionCond<T> for O {
    fn and_if_flat<F: FnOnce(T) -> Option<bool>>(self, pred: F) -> bool {
        self.into_option().and_then(pred).unwrap_or(false)
    }
    fn or_if_flat<F: FnOnce(T) -> Option<bool>>(self, pred: F) -> bool {
        self.into_option().and_then(pred).unwrap_or(true)
    }
    fn and_if<F: FnOnce(T) -> bool>(self, pred: F) -> bool {
        self.into_option().map(pred).unwrap_or(false)
    }
    fn or_if<F: FnOnce(T) -> bool>(self, pred: F) -> bool {
        self.into_option().map(pred).unwrap_or(true)
    }
}

impl<T> Optional<T> for Option<T> {
    fn into_option(self) -> Self {
        self
    }
}

impl<T, E> Optional<T> for Result<T, E> {
    fn into_option(self) -> Option<T> {
        self.ok()
    }
}

fn main_loop(config: Config, mut state: State) -> Result<()> {
    let ticker = tick(config.interval.to_std().unwrap());
    let exit = exit_channel()?;
    let (_watcher, hosts_modified) = hosts_modified_channel()?;
    let (_socket, unlock_request) = unlock_request_channel()?;

    daemonize()?;

    loop {
        select! {
            recv(ticker) -> _ => {
                if let Err(e) = state.update(&config) {
                    for e in e {
                        println!("{:?}", e);
                    }
                }
            },
            recv(exit) -> _ => {
                if let Err(e) = fs::write("/var/lib/senklot", state.export()) {
                    println!("{:?}", e);
                }
                return Ok(());
            },
            recv(hosts_modified) -> _ => {
                if let Err(e) = state.commit() {
                    println!("{:?}", e);
                }
            },
            recv(unlock_request) -> msg => {
                if let Ok(name) = msg {
                   if let Err(e)= state.unlock(&name, &config.entries[&name], &config.after_unlock) {
                    println!("{:?}", e);
                   }
                }
            }
        }
    }
}

fn run_as_daemon(config: Config) -> Result<()> {
    let state = read_state_file().context("Unable to read state state file")?;
    let state = State::load_with(&config, state);

    main_loop(config, state)?;

    Ok(())
}

fn run_unlock(_: Config, name: &str) -> Result<()> {
    let mut stream = net::UnixStream::connect("/var/lib/senklot.socket")?;
    stream.write_all(name.as_bytes())?;
    Ok(())
}

fn main() -> Result<()> {
    let arg = clap::App::new(clap::crate_name!())
        .version(clap::crate_version!())
        .subcommand(clap::SubCommand::with_name("start").about("Start a daemonized process"))
        .subcommand(
            clap::SubCommand::with_name("unlock")
                .about("Unlock the access to given contents")
                .arg_from_usage("<NAME> 'Name of contents to be unlocked'"),
        )
        .help_message("Print help message")
        .version_message("Print version message")
        .version_short("v")
        .setting(clap::AppSettings::UnifiedHelpMessage)
        .setting(clap::AppSettings::VersionlessSubcommands)
        .setting(clap::AppSettings::SubcommandRequiredElseHelp)
        .get_matches_safe()
        .map_err(|mut e| {
            if let clap::ErrorKind::MissingArgumentOrSubcommand
            | clap::ErrorKind::HelpDisplayed
            | clap::ErrorKind::VersionDisplayed = e.kind
            {
                e.exit();
            }
            e.message = e.message.get(7..).unwrap_or("").to_owned();
            e
        })?;
    let config = read_config_file().context("Unable to read config")?;
    let config = parse_config(&config).context("Parse error in config")?;

    if let Some(_) = arg.subcommand_matches("start") {
        run_as_daemon(config)?;
        Ok(())
    } else if let Some(arg) = arg.subcommand_matches("unlock") {
        run_unlock(config, arg.value_of("NAME").unwrap())?;
        Ok(())
    } else {
        unreachable!();
    }
}
