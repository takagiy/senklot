use anyhow::{Context, Result};
use crossbeam::channel;
use crossbeam::channel::{select, tick};
use daemonize::Daemonize;
use notify::event::*;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::borrow::ToOwned;
use std::fs;
use std::fs::File;
use std::io::prelude::*;
use std::os::unix::net;
use std::path::Path;

mod config;
mod state;
mod util;

use config::*;
use state::*;
use util::*;

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

fn main_loop(config: Config, mut state: State) -> Result<()> {
    let ticker = tick(config.interval.to_std().unwrap());
    let (_watcher, hosts_modified) = hosts_modified_channel()?;
    let (_socket, unlock_request) = unlock_request_channel()?;

    daemonize()?;

    let exit = exit_channel()?;

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

fn read_config_file() -> Result<String> {
    let config_file = "/etc/senklot/config";
    let content = fs::read_to_string(config_file)?;
    Ok(content)
}

fn parse_config(config: &str) -> Result<Config> {
    let config = toml::from_str(config)?;
    Ok(config)
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
