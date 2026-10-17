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
    /// Without a subcommand, `td` opens the TUI.
    #[command(subcommand)]
    pub cmd: Option<Command>,
}

/// The CLI surface doubles as the socket protocol: a client sends one of these
/// as a JSON line, the daemon streams back `Event`s.
#[derive(Subcommand, Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    /// Run as a service: listen on the socket and sync on an interval.
    Daemon,
    /// Browse the library and playlists in the terminal, with actions. Also what plain `td` does.
    Tui,
    /// Fetch ListenBrainz playlists, download missing tracks, write m3u files, push to sync targets.
    Sync {
        /// Only this playlist kind (e.g. weekly-jams).
        #[arg(long)]
        playlist: Option<String>,
        /// Skip pushing to sync targets.
        #[arg(long)]
        no_push: bool,
        /// Skip ListenBrainz: only download what is already wanted, then refresh and push the
        /// playlists those tracks belong to.
        #[arg(long)]
        #[serde(default)]
        no_fetch: bool,
    },
    /// Library counts by track status.
    Status,
    /// Search tracks by artist, album or title.
    Search { query: String },
    /// Playlists and where they have been synced.
    Playlists,
    /// Push a playlist to the sync targets: a fetched one by id or kind, or any .m3u8 file
    /// (the archive, a backup, your own). A same-titled remote playlist is backed up, then replaced.
    Push {
        /// Playlist id, kind, or path to an .m3u8 file.
        playlist: String,
        /// Only this target (e.g. plex).
        #[arg(long)]
        to: Option<String>,
        /// Title on the target (files only; default: the file's #PLAYLIST line, else its name).
        #[arg(long)]
        title: Option<String>,
    },
    /// Remove a playlist from the sync targets, backing it up first. The local file is never touched.
    Unpush {
        /// Remote title, or an .m3u8 file whose remote copy to remove.
        playlist: String,
        /// Only this target (e.g. plex).
        #[arg(long)]
        to: Option<String>,
        /// Actually delete; without it, only list what matches.
        #[arg(long)]
        yes: bool,
    },
    /// Reset failed tracks to wanted so the next sync retries them; with an id, re-download that
    /// one track (its current file is dropped).
    Retry { track: Option<i64> },
    /// Delete a track's file and forget it. A later sync re-adds it if a playlist still lists it.
    Rm { track: i64 },
    /// Pin the download URL for a track (see `td search`) and queue it for the next sync.
    Source { track: i64, url: String },
    /// Re-query MusicBrainz for every track, retag, and move files whose templated path changed.
    Refile {
        /// Report what would move without touching anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Playlists that live on a sync target (list, bulk delete).
    Remote {
        #[command(subcommand)]
        action: RemoteCmd,
    },
    /// Bring an existing folder of music (e.g. Explo's downloads) into the library.
    /// Dry run unless --apply; files are moved, not copied.
    /// Bring a folder of audio files into the library. Dry run by default; --apply copies the files
    /// in and leaves the source alone; --move moves them instead.
    Import {
        dir: PathBuf,
        /// Copy the files into the library (default is a dry run that changes nothing).
        #[arg(long)]
        apply: bool,
        /// Move instead of copy: files leave the source folder. Needs write access there.
        #[arg(long, requires = "apply")]
        #[serde(default)]
        r#move: bool,
        /// Accepted for compatibility; copying is the default.
        #[arg(long, hide = true, conflicts_with = "move")]
        #[serde(default)]
        copy: bool,
        /// With --move: delete source files whose recording is already in the library.
        #[arg(long, requires = "move")]
        delete_duplicates: bool,
    },
}

#[derive(Subcommand, Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RemoteCmd {
    /// List playlists on the target(s).
    Ls {
        #[arg(long)]
        to: Option<String>,
    },
    /// Delete playlists whose title matches a glob (`*`, `?`). Only lists them unless --yes.
    Rm {
        pattern: String,
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        yes: bool,
        /// Also match the server's own smart playlists.
        #[arg(long)]
        include_smart: bool,
        /// Skip the m3u8 + JSON backup normally written before deleting.
        #[arg(long)]
        no_backup: bool,
        /// Backup directory (default: <playlist_dir>/backup).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Save matching playlists as m3u8 + JSON (what `rm` does before deleting).
    Backup {
        #[arg(default_value = "*")]
        pattern: String,
        #[arg(long)]
        to: Option<String>,
        /// Output directory (default: <playlist_dir>/backup).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Recreate a playlist from a JSON file written by `backup`.
    Restore {
        file: PathBuf,
        /// Target to restore to (default: the one the backup came from).
        #[arg(long)]
        to: Option<String>,
    },
}
