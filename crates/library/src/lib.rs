//! FramePlayer's media library.
//!
//! A SQLite (WAL) database in `$XDG_DATA_HOME/frameplayer/library.sqlite`
//! indexing videos from any [`fp_sources::Source`], plus everything users
//! attach to them: per-video view overrides (keyed by content hash + path,
//! so they survive moves and rescans), tags, ratings, favourites, resume
//! points, watch history, named bookmarks, playlists and smart playlists,
//! thumbnails / preview sprites and funscripts.
//!
//! Entry points for the app:
//! - [`Library::open`] / [`Library::open_default`] / [`Library::open_in_memory`]
//! - [`Indexer`] scans a source (inject a [`Thumbnailer`] for thumbnails)
//! - [`ItemQuery`] + [`Library::query`] for search, filters and sorting
//! - [`export::export_library`] / [`export::import_library`] for backups

mod db;
mod error;
pub mod export;
pub mod funscript;
pub mod fuzzy;
pub mod hash;
pub mod indexer;
mod items;
mod model;
mod playlists;
pub mod query;
mod userdata;

pub use db::{default_db_path, Library, SCHEMA_VERSION};
pub use error::{LibraryError, Result};
pub use funscript::ScriptRef;
pub use indexer::{
    IndexProgress, IndexReport, Indexer, SpriteInfo, SpriteSpec, ThumbnailJob, ThumbnailOutput,
    Thumbnailer,
};
pub use model::{Bookmark, DetectSource, HistoryEntry, Item, NewItem, Playlist, ProjectionKind};
pub use query::{ItemQuery, ScoredItem, Sort};
