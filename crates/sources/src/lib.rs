//! Media sources for FramePlayer.
//!
//! Every place a video can live (local disks, removable media, SMB shares,
//! WebDAV, plain HTTP(S), HLS/DASH streams, DLNA servers, SFTP and
//! DeoVR-compatible JSON feeds) is exposed through one async [`Source`]
//! trait: list a directory, open a file for random access. Opened files
//! implement [`RandomAccess`]; [`BlockingReader`] adapts those to
//! `std::io::Read + Seek` with read-ahead so a demuxer running on its own
//! (non-tokio) thread can consume them.
//!
//! Entry points for the app:
//! - [`SourceConfig`] + [`connect`] build a `Arc<dyn Source>` from stored config.
//! - [`CredentialStore`] keeps per-source secrets encrypted at rest.
//! - [`local::MountWatcher`] reports removable drives appearing/disappearing.
//! - [`dlna::discover_media_servers`] finds UPnP media servers on the LAN.
//! - [`deovr::DeoVrClient`] reads XBVR/Stash style feeds.
//! - [`stream`] resolves HLS/DASH manifests into variants and segment streams.

pub mod blocking;
pub mod config;
pub mod credentials;
pub mod dash;
pub mod deovr;
pub mod dlna;
mod error;
pub mod hls;
pub mod http;
pub mod local;
pub mod segments;
pub mod sftp;
pub mod smb;
mod source;
pub mod stream;
pub mod webdav;
pub mod xml;

pub use blocking::{BlockingReader, BlockingSegmentReader, ReadAheadConfig};
pub use config::{connect, kind_for_uri, SourceConfig, SourceKind};
pub use credentials::{CredentialStore, Credentials};
pub use error::{Result, SourceError};
pub use source::{
    is_script_name, is_video_name, read_all, walk, Entry, MemoryFile, RandomAccess, Source,
    VIDEO_EXTENSIONS,
};
