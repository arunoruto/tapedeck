use std::path::PathBuf;

use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

#[derive(Parser, Debug)]
#[command(name = "td", version, about = "ListenBrainz → downloads → tagged library → Plex")]
pub struct Cli {
    /// Emit newline-delimited JSON events instead of human output.
    #[arg(long, global = true)]
    pub json: bool,
    /// Config file (default: $XDG_CONFIG_HOME/tapedeck/config.toml).
    #[arg(long, global = true, env = "TAPEDECK_CONFIG")]
    pub config: Option<PathBuf>,
    #[command(subcommand)]
    pub cmd: Command,
}

/// The CLI surface doubles as the socket protocol: a client sends one of these
/// as a JSON line, the daemon streams back `Event`s.
#[derive(Subcommand, Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    /// Run as a service: listen on the socket and sync on an interval.
    Daemon,
    /// Fetch ListenBrainz playlists, download missing tracks, write m3u files, push to sync targets.
    Sync {
        /// Only this playlist kind (e.g. weekly-jams).
        #[arg(long)]
        playlist: Option<String>,
        /// Skip pushing to sync targets.
        #[arg(long)]
        no_push: bool,
    },
    /// Library counts by track status.
    Status,
    /// Search tracks by artist, album or title.
    Search { query: String },
    /// Playlists and where they have been synced.
    Playlists,
    /// Push one playlist to the sync targets.
    Push {
        /// Playlist id or kind.
        playlist: String,
        /// Only this target (e.g. plex).
        #[arg(long)]
        to: Option<String>,
    },
    /// Reset failed tracks to wanted so the next sync retries them.
    Retry,
}
