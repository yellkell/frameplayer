//! Installer errors, each with a plain-language suggestion for the fix.

use std::path::PathBuf;

use crate::vdf::VdfError;

/// Everything that can stop the installer.
#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    /// `ssh` or `scp` is not installed or not on `PATH`.
    #[error("the `{0}` program was not found on this computer")]
    SshMissing(String),
    /// The `--host` value cannot be passed to ssh safely.
    #[error("{0:?} is not a usable host (expected an ssh alias like `frame` or user@address)")]
    BadHost(String),
    /// An `--ssh-option` is not `Key=Value`.
    #[error("ssh option {0:?} must look like Key=Value (for example Port=2222)")]
    BadSshOption(String),
    /// ssh could not connect or log in.
    #[error("could not connect to the headset `{host}`: {detail}")]
    Connect {
        /// Host or alias used.
        host: String,
        /// ssh's own message.
        detail: String,
    },
    /// A command on the headset failed.
    #[error("{what} failed on the headset (exit code {code:?}): {detail}")]
    Remote {
        /// What we were doing.
        what: String,
        /// Exit code.
        code: Option<i32>,
        /// Last lines of its output.
        detail: String,
    },
    /// Copying a file with scp failed.
    #[error("copying {what} failed: {detail}")]
    Copy {
        /// What was being copied.
        what: String,
        /// scp's message.
        detail: String,
    },
    /// The device is not a Steam Frame (or not ARM64 Linux).
    #[error("this does not look like a Steam Frame: {0}")]
    WrongDevice(String),
    /// Something on the headset is missing (unzip/python3, disk space).
    #[error("{0}")]
    Requirement(String),
    /// A path given or found is unusable.
    #[error("unusable path: {0}")]
    BadPath(String),
    /// Checking, downloading or reading the release failed.
    #[error(transparent)]
    Updater(#[from] fp_updater::Error),
    /// `shortcuts.vdf` could not be read or written.
    #[error("cannot edit Steam's shortcut list {path}: {source}")]
    Vdf {
        /// File on the headset.
        path: String,
        /// Parse or write error.
        #[source]
        source: VdfError,
    },
    /// A local file operation failed.
    #[error("{action} {path}: {source}")]
    Io {
        /// What was attempted.
        action: &'static str,
        /// Local path.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// No release could be found to install.
    #[error("{0}")]
    NoRelease(String),
}

/// Result alias for the installer.
pub type Result<T, E = InstallError> = std::result::Result<T, E>;

impl InstallError {
    /// What the user can do about it, in plain words.
    pub fn hint(&self) -> Option<String> {
        Some(match self {
            InstallError::SshMissing(_) => "Install the OpenSSH client.\n\
                 - Windows 10/11: Settings > System > Optional features > Add \"OpenSSH Client\".\n\
                 - macOS: it is built in; check that /usr/bin is on your PATH.\n\
                 - Linux: install the openssh-client (Debian/Ubuntu) or openssh package."
                .into(),
            InstallError::Connect { host, .. } => format!(
                "Check that:\n\
                 - the headset is switched on, awake and on the same network as this computer;\n\
                 - Developer Mode is on (Settings > System > Developer Mode);\n\
                 - the headset is paired: pair it once with Frame Control (which creates the \
                 ssh alias `frame`), FrameDrop or Valve's SteamOS Devkit Client;\n\
                 - if you paired another way, pass --host user@address (current: {host}), and \
                 --ssh-option IdentityFile=<key> if the key is not your default one."
            ),
            InstallError::BadHost(_) => {
                "Use --host frame (Frame Control's alias) or --host user@192.168.x.y.".into()
            }
            InstallError::WrongDevice(_) => {
                "Make sure --host points at the Steam Frame. Use --force to install anyway.".into()
            }
            InstallError::Updater(fp_updater::Error::NoPublicKey) => {
                "This installer build cannot verify downloads yet. Download the FramePlayer \
                 zip from the releases page and run: frameplayer-install --zip <file.zip>"
                    .into()
            }
            InstallError::Updater(
                fp_updater::Error::SignatureMismatch | fp_updater::Error::MalformedSignature(_),
            ) => "The release information failed its signature check, so nothing was \
                  installed. Try again later, or report this to the FramePlayer project."
                .into(),
            InstallError::Updater(fp_updater::Error::ChecksumMismatch { .. }) => {
                "The download was corrupted; run the installer again to re-download.".into()
            }
            InstallError::Updater(fp_updater::Error::Http { .. }) => {
                "Check this computer's internet connection, or download the zip yourself \
                 and pass --zip <file.zip>."
                    .into()
            }
            InstallError::Vdf { .. } => {
                "Your Steam shortcut list was left unchanged. You can add FramePlayer by \
                 hand: Steam > Add a Non-Steam Game, and pick frameplayer.sh in the install \
                 folder."
                    .into()
            }
            InstallError::Copy { .. } => {
                "Check that the headset has free space and is still connected, then retry.".into()
            }
            _ => return None,
        })
    }

    pub(crate) fn io(
        action: &'static str,
        path: impl Into<PathBuf>,
        source: std::io::Error,
    ) -> Self {
        InstallError::Io {
            action,
            path: path.into(),
            source,
        }
    }
}
