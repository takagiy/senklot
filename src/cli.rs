use anyhow::Result;
use structopt::clap::AppSettings::*;
use structopt::clap::ErrorKind::*;
use structopt::StructOpt;

#[derive(StructOpt)]
pub enum Args {
    Start {},
    Unlock { name: String },
}

pub fn get_args() -> Result<Args> {
    let matches = Args::clap()
        .help_message("Print help message")
        .version_message("Print version message")
        .version_short("v")
        .setting(UnifiedHelpMessage)
        .setting(VersionlessSubcommands)
        .setting(SubcommandRequiredElseHelp)
        .get_matches_safe()
        .map_err(|mut e| {
            if matches!(
                e.kind,
                HelpDisplayed | VersionDisplayed | MissingArgumentOrSubcommand
            ) {
                e.exit();
            }
            e.message = e.message.get(7..).unwrap_or("").to_owned();
            e
        })?;

    Ok(Args::from_clap(&matches))
}
