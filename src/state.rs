use anyhow::{anyhow, Result};
use chrono::offset::Local;
use nom::character::complete::{none_of, space0, space1};
use nom::{alt, many1, map, named, recognize, tag, tuple};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;

use crate::config::*;
use crate::util::*;

#[derive(Deserialize, Serialize)]
pub struct State {
    last_unlocked: HashMap<String, LocalTime>,
    last_locked: HashMap<String, LocalTime>,
    is_locked: HashMap<String, bool>,
    #[serde(skip)]
    domain_map: HashMap<String, String>,
    #[serde(skip)]
    path: PathBuf,
}

fn read_state_file(path: &str) -> Result<Option<Vec<u8>>> {
    let path = Path::new(path);
    if path.is_file() {
        let content = fs::read(path)?;
        Ok(Some(content))
    } else {
        Ok(None)
    }
}

impl State {
    pub fn read_with_config(config: &Config, path: &str) -> Result<State> {
        let domain_map = {
            let mut domain_map = HashMap::new();

            for (name, entry) in &config.entries {
                for domain in &entry.domains {
                    domain_map.insert(domain.clone(), name.clone());
                }
            }
            domain_map
        };

        let previous_state = {
            let previous_state = read_state_file(path)?;

            match previous_state {
                Some(state) => bincode::deserialize(&state).unwrap_or(State::empty()),
                None => State::empty(),
            }
        };

        Ok(State {
            domain_map: domain_map,
            path: path.to_owned().into(),
            ..previous_state
        })
    }

    fn empty() -> State {
        State {
            domain_map: HashMap::new(),
            last_unlocked: HashMap::new(),
            last_locked: HashMap::new(),
            is_locked: HashMap::new(),
            path: PathBuf::new(),
        }
    }

    pub fn export(&self) -> Vec<u8> {
        bincode::serialize(&self).unwrap()
    }

    pub fn unlock(
        &mut self,
        name: &str,
        entry: &Entry,
        after_unlock: &Option<String>,
    ) -> Result<()> {
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
            excute_command(cmd, name)?;
        }

        Ok(())
    }

    pub fn lock(&mut self, name: &str, entry: &Entry, after_lock: &Option<String>) -> Result<()> {
        if self.is_locked.get(name).and_if(|is_locked| *is_locked) {
            return Ok(());
        }

        self.is_locked.set(name, true);

        if matches!(entry.restriction, Restriction::Dynamic{..}) {
            self.last_locked.set(name, Local::now());
        }

        self.commit()?;

        if let Some(cmd) = after_lock {
            excute_command(cmd, name)?;
        }

        Ok(())
    }

    fn domanin_is_locked(&self, domain: &str) -> bool {
        self.domain_map
            .get(domain)
            .and_if_flat(|entry| self.is_locked.get(entry).cloned())
    }

    fn save(&self) -> Result<()> {
        fs::write(&self.path, self.export())?;
        Ok(())
    }

    pub fn commit(&self) -> Result<()> {
        let (hosts, state_is_changed) = {
            let hosts = read_hosts()?;
            let mut hosts = Hosts::parse(hosts);
            let mut state_is_changed = false;

            for domain in self.domain_map.keys() {
                let lock_state = self.domanin_is_locked(domain);

                if lock_state != hosts.is_locked(domain) {
                    state_is_changed = true;
                    hosts.write_state(domain, lock_state);
                }
            }
            (hosts, state_is_changed)
        };

        if !state_is_changed {
            return Ok(());
        }

        hosts.save()?;
        self.save()?;

        Ok(())
    }

    pub fn update(&mut self, config: &Config) -> Result<(), Vec<anyhow::Error>> {
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

    fn export(&self) -> String {
        self.hosts_file.join("\n")
    }

    fn save(&self) -> Result<()> {
        fs::write("/etc/hosts", self.export())?;
        Ok(())
    }
}

fn read_hosts() -> Result<String> {
    let content = fs::read_to_string("/etc/hosts")?;
    Ok(content)
}

fn excute_command(command: &str, content_name: &str) -> Result<()> {
    process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .env("SENKLOT_CONTENT", content_name)
        .spawn()?;
    Ok(())
}
