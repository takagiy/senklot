use anyhow::{Context, Result};
use crossbeam::channel;
use crossbeam::channel::{select, tick};
use daemonize::Daemonize;
use notify::event::*;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::fs;
use std::fs::File;
use std::io::prelude::*;
use std::os::unix::net;
use std::net::Shutdown;

mod cli;
mod config;
mod message;
mod state;
mod util;

use cli::*;
use config::*;
use message::*;
use state::*;
use util::*;

fn main() -> Result<()> {
    let args = get_args()?;

    let config = read_config_file().context("Unable to read config")?;
    let config = parse_config(&config).context("Parse error in config")?;

    match args {
        Args::Start {} => run_as_daemon(config),
        Args::Unlock { name } => run_unlock(config, &name),
    }
}

fn run_as_daemon(config: Config) -> Result<()> {
    let state = State::read_with_config(&config, "/var/lib/senklot")
        .context("Unable to read state file")?;

    main_loop(config, state)?;

    Ok(())
}

fn run_unlock(_: Config, name: &str) -> Result<()> {
    let mut stream = net::UnixStream::connect("/var/lib/senklot.socket")?;
    stream.write_all(name.as_bytes())?;
    stream.shutdown(Shutdown::Write)?;
    let response = {
        let mut response = String::new();
        stream.read_to_string(&mut response)?;
        bincode::deserialize(&response.as_bytes())?
    };

    match response {
        UnlockResponse::Success { locked_at } => {
            println!("{}", locked_at);
        }
        UnlockResponse::Fail { cause, unlocked_at } => {
            println!(
                "{}\n{}",
                cause,
                unlocked_at
                    .map(|t| format!("{}", t))
                    .as_deref()
                    .unwrap_or("")
            );
        }
    }

    Ok(())
}

fn main_loop(config: Config, mut state: State) -> Result<()> {
    let channels = daemonize()?;
    let ticker = tick(config.interval.to_std().unwrap());
    let (_watcher, hosts_modified) = channels.hosts_modified;
    let (_socket, unlock_request) = channels.unlock_request;
    let exit = channels.exit;

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
                if let Ok((socket, name)) = msg {
                    if let Err(e)= state.request_unlock(socket, &name, &config.entries[&name], &config.after_unlock) {
                        println!("{:?}", e);
                    }
                }
            }
        }
    }
}

fn daemonize() -> Result<Channels> {
    fs::create_dir_all("/tmp/senklot")?;

    let stdout = File::create("/tmp/senklot/stdout.log")
        .context("Unable to open /tmp/senklot/stdout.log")?;
    let stderr = File::create("/tmp/senklot/stderr.log")
        .context("Unable to open /tmp/senklot/stderr.log")?;

    let channels = Daemonize::new()
        .stdout(stdout)
        .stderr(stderr)
        .pid_file("/tmp/senklot/senklot.pid")
        .privileged_action(prepare_channels)
        .start()
        .context("Unable to start daemon")?;

    channels
}

fn prepare_channels() -> Result<Channels> {
    Ok(Channels {
        exit: exit_channel()?,
        hosts_modified: hosts_modified_channel()?,
        unlock_request: unlock_request_channel()?,
    })
}

struct Channels {
    exit: channel::Receiver<()>,
    hosts_modified: (RecommendedWatcher, channel::Receiver<()>),
    unlock_request: (SocketPath, channel::Receiver<(net::UnixStream, String)>),
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

fn unlock_request_channel() -> Result<(SocketPath, channel::Receiver<(net::UnixStream, String)>)> {
    let (tx, rx) = channel::bounded(0);
    let (path, listener) = SocketPath::bind("/var/lib/senklot.socket")?;
    path.allow_write()?;
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if let Ok(mut stream) = stream {
                let mut buffer = String::new();
                if stream.read_to_string(&mut buffer).is_ok() {
                    let _ = tx.send((stream, buffer));
                };
            }
        }
    });
    Ok((path, rx))
}

fn read_config_file() -> Result<String> {
    let config_file = "/etc/senklot/config";
    let content = fs::read_to_string(config_file)?;
    Ok(content)
}

fn parse_config(config: &str) -> Result<Config> {
    let config = toml::from_str(config)?;
    Ok(config)
}
