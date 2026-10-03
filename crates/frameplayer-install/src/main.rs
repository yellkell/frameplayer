//! Command-line entry point for `frameplayer-install`.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use fp_updater::{Channel, DEFAULT_MANIFEST_URL};
use frameplayer_install::ops::{
    self, DEFAULT_INSTALL_DIR, InstallOptions, ShortcutMethod, UninstallOptions,
};
use frameplayer_install::remote::SshRemote;
use frameplayer_install::shortcut::DEFAULT_APP_NAME;
use frameplayer_install::{InstallError, Result};

/// Install FramePlayer on a Steam Frame over SSH.
///
/// Before the first run: turn on Developer Mode on the headset and pair it
/// once with Frame Control (or FrameDrop, or Valve's SteamOS Devkit
/// Client). Then simply run `frameplayer-install`.
#[derive(Debug, Parser)]
#[command(version, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[command(flatten)]
    install: InstallArgs,
}

/// How to reach the headset (accepted by every command).
#[derive(Debug, Args)]
struct Connection {
    /// The headset: an ssh alias (Frame Control creates `frame`) or
    /// user@address.
    #[arg(long, default_value = "frame")]
    host: String,

    /// Extra ssh setting, e.g. `Port=2222` or `IdentityFile=~/.ssh/frame`
    /// (repeatable).
    #[arg(long = "ssh-option", value_name = "KEY=VALUE")]
    ssh_options: Vec<String>,
}

impl Connection {
    fn remote(&self) -> Result<SshRemote> {
        let remote = SshRemote::new(&self.host, &self.ssh_options)?;
        remote.check_tools()?;
        Ok(remote)
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Install or update FramePlayer (the default when no command is given).
    Install(InstallArgs),
    /// Remove FramePlayer and its Steam library entry.
    Uninstall(UninstallArgs),
    /// Show what is installed on the headset.
    Status(StatusArgs),
}

#[derive(Debug, Args)]
struct InstallArgs {
    #[command(flatten)]
    conn: Connection,

    /// Install this release zip instead of downloading the latest one.
    #[arg(long, value_name = "FILE")]
    zip: Option<PathBuf>,

    /// Install folder on the headset, relative to the home folder.
    /// Devkit-style installs use `devkit-game/frameplayer`.
    #[arg(long, default_value = DEFAULT_INSTALL_DIR, value_name = "FOLDER")]
    dir: String,

    /// Release channel to download from.
    #[arg(long, default_value = "stable", value_parser = parse_channel)]
    channel: Channel,

    /// Signed release manifest to download from.
    #[arg(long, default_value = DEFAULT_MANIFEST_URL, value_name = "URL")]
    manifest_url: String,

    /// Name shown in the Steam library.
    #[arg(long, default_value = DEFAULT_APP_NAME)]
    name: String,

    /// How to add FramePlayer to the Steam library.
    #[arg(long, value_enum, default_value_t = ShortcutMethod::Auto)]
    shortcut: ShortcutMethod,

    /// Restart Steam afterwards (runs `steam -shutdown`; Gaming Mode
    /// starts it again) so the new library entry appears right away.
    #[arg(long)]
    restart_steam: bool,

    /// Copy the files even if this version is already installed.
    #[arg(long)]
    reinstall: bool,

    /// Install even if the device does not look like a Steam Frame.
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Args)]
struct UninstallArgs {
    #[command(flatten)]
    conn: Connection,

    /// Install folder on the headset, relative to the home folder.
    #[arg(long, default_value = DEFAULT_INSTALL_DIR, value_name = "FOLDER")]
    dir: String,

    /// Name FramePlayer was given in the Steam library.
    #[arg(long, default_value = DEFAULT_APP_NAME)]
    name: String,

    /// Also delete FramePlayer's settings, library database and caches.
    #[arg(long)]
    purge: bool,

    /// Restart Steam afterwards so the library updates right away.
    #[arg(long)]
    restart_steam: bool,

    /// Run even if the device does not look like a Steam Frame.
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Args)]
struct StatusArgs {
    #[command(flatten)]
    conn: Connection,

    /// Install folder on the headset, relative to the home folder.
    #[arg(long, default_value = DEFAULT_INSTALL_DIR, value_name = "FOLDER")]
    dir: String,
}

fn parse_channel(s: &str) -> std::result::Result<Channel, String> {
    s.parse()
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("\nError: {e}");
            if let Some(hint) = e.hint() {
                eprintln!("\n{hint}");
            }
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        None => install(cli.install),
        Some(Command::Install(args)) => install(args),
        Some(Command::Uninstall(args)) => {
            let mut remote = args.conn.remote()?;
            let device = ops::connect(&mut remote, args.force)?;
            ops::uninstall(
                &mut remote,
                &device,
                &UninstallOptions {
                    dir: args.dir,
                    app_name: args.name,
                    purge: args.purge,
                    restart_steam: args.restart_steam,
                },
            )
        }
        Some(Command::Status(args)) => {
            let mut remote = args.conn.remote()?;
            let device = ops::connect(&mut remote, true)?;
            ops::status(&mut remote, &device, &args.dir).map(|_| ())
        }
    }
}

fn install(args: InstallArgs) -> Result<()> {
    // Connect first: a pairing problem should show up before a long
    // download.
    let mut remote = args.conn.remote()?;
    let device = ops::connect(&mut remote, args.force)?;
    let zip = match args.zip {
        Some(z) => {
            if !z.is_file() {
                return Err(InstallError::BadPath(format!(
                    "{} is not a file",
                    z.display()
                )));
            }
            z
        }
        None => {
            let cache = std::env::temp_dir().join("frameplayer-install");
            ops::fetch_latest(&args.manifest_url, args.channel, &cache)?
        }
    };
    let version = ops::install_zip(
        &mut remote,
        &device,
        &zip,
        &InstallOptions {
            dir: args.dir,
            app_name: args.name.clone(),
            shortcut: args.shortcut,
            restart_steam: args.restart_steam,
            reinstall: args.reinstall,
        },
    )?;
    println!(
        "\nDone. FramePlayer {version} is on the headset: find \"{}\" in the Steam library \
         (Library > Non-Steam).",
        args.name
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn default_command_is_install() {
        let cli = Cli::try_parse_from(["frameplayer-install"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.install.conn.host, "frame");
        assert_eq!(cli.install.dir, "frameplayer");
        assert_eq!(cli.install.channel, Channel::Stable);
        assert_eq!(cli.install.manifest_url, DEFAULT_MANIFEST_URL);

        let cli = Cli::try_parse_from([
            "frameplayer-install",
            "--host",
            "steam@192.168.1.20",
            "--zip",
            "fp.zip",
            "--channel",
            "beta",
            "--shortcut",
            "vdf",
            "--restart-steam",
        ])
        .unwrap();
        assert_eq!(cli.install.conn.host, "steam@192.168.1.20");
        assert_eq!(cli.install.zip, Some(PathBuf::from("fp.zip")));
        assert_eq!(cli.install.channel, Channel::Beta);
        assert_eq!(cli.install.shortcut, ShortcutMethod::Vdf);
        assert!(cli.install.restart_steam);
    }

    #[test]
    fn subcommands_parse() {
        let cli = Cli::try_parse_from([
            "frameplayer-install",
            "uninstall",
            "--purge",
            "--host",
            "deck",
        ])
        .unwrap();
        match cli.command {
            Some(Command::Uninstall(a)) => {
                assert!(a.purge);
                assert_eq!(a.dir, "frameplayer");
                assert_eq!(a.conn.host, "deck");
            }
            other => panic!("{other:?}"),
        }
        let cli = Cli::try_parse_from([
            "frameplayer-install",
            "status",
            "--ssh-option",
            "Port=2222",
            "--dir",
            "devkit-game/frameplayer",
        ])
        .unwrap();
        match cli.command {
            Some(Command::Status(a)) => {
                assert_eq!(a.conn.ssh_options, ["Port=2222"]);
                assert_eq!(a.dir, "devkit-game/frameplayer");
            }
            other => panic!("{other:?}"),
        }
        // Install options belong to install, not to other commands.
        assert!(Cli::try_parse_from(["frameplayer-install", "--zip", "a.zip", "status"]).is_err());
        assert!(Cli::try_parse_from(["frameplayer-install", "--channel", "nightly"]).is_err());
    }
}
