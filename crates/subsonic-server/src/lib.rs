//! Serve any music library to Subsonic clients. Implement [`Library`] (songs, playlists,
//! files, cover art, scrobbles) and mount [`Server::router`]; this crate does the protocol:
//! request parsing, the `u`/`t`/`s` sign-in, the XML/JSON/JSONP envelope and error codes,
//! grouping your flat song list into the artist and album views, streaming with Range, and
//! the empty answers clients expect for the calls a plain library has nothing for.
//!
//! Grown inside tapedeck and kept free of its types so it can live on its own.

pub mod auth;
mod render;
mod server;
mod shelf;

use std::path::PathBuf;

pub use auth::token;
pub use server::{Server, Trusted, parse_lrc};

/// A playable track. `id`s are yours and opaque to the protocol; `album_id` and `artist_id`
/// group songs into the album and artist views, so give every song of an album the same
/// `album_id` and every album of an artist the same `artist_id`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Song {
    pub id: String,
    pub title: String,
    /// Track artist credit, as shown next to the title.
    pub artist: String,
    /// Album artist; the artist view groups by this (and by `artist_id`).
    pub album_artist: String,
    pub album: String,
    pub album_id: String,
    pub artist_id: String,
    pub track: Option<i64>,
    pub year: Option<i64>,
    /// Seconds.
    pub duration: Option<i64>,
    /// Display path, relative to the library; its extension decides the content type.
    pub path: String,
    /// The id clients pass to `getCoverArt`, when there is art for this song's album.
    pub cover_art: Option<String>,
    pub music_brainz_id: Option<String>,
    pub album_music_brainz_id: Option<String>,
    pub artist_music_brainz_id: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    pub comment: Option<String>,
    pub owner: Option<String>,
    /// ISO 8601 timestamps.
    pub created: Option<String>,
    pub changed: Option<String>,
    pub song_count: i64,
    /// Seconds.
    pub duration: i64,
    pub cover_art: Option<String>,
}

/// Lyrics of a song: LRC text with `[mm:ss.xx]` stamps, plain text, or both.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Lyrics {
    pub synced: Option<String>,
    pub plain: Option<String>,
}

/// What an anonymous request wants to do; see [`Library::anonymous_may`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Browsing, searching, playlists.
    Read,
    /// Audio and cover art.
    Stream,
    /// Scrobbles.
    Listen,
}

/// A Subsonic error: HTTP 200, `status="failed"`, one of the protocol's codes.
#[derive(Debug, Clone, PartialEq)]
pub struct Error {
    pub code: u16,
    pub message: String,
}

impl Error {
    pub fn new(code: u16, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }

    /// Code 0, for anything that went wrong on your side.
    pub fn internal(e: impl std::fmt::Display) -> Self {
        Self::new(0, e.to_string())
    }

    /// Code 70, "requested data not found".
    pub fn not_found(what: impl Into<String>) -> Self {
        Self::new(70, what)
    }

    /// Code 10, "required parameter missing".
    pub fn missing(param: &str) -> Self {
        Self::new(10, format!("missing parameter {param}"))
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "subsonic error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for Error {}

/// Your music, as the protocol needs it. Only the first five methods are required.
#[async_trait::async_trait]
pub trait Library: Send + Sync + 'static {
    /// Every playable song. The server groups them into albums and artists on each request,
    /// so keep this cheap or cache it; a library of a few thousand songs needs no cache.
    async fn songs(&self) -> Result<Vec<Song>, Error>;

    async fn song(&self, id: &str) -> Result<Option<Song>, Error>;

    async fn playlists(&self) -> Result<Vec<Playlist>, Error>;

    /// The playable songs of a playlist, in order; `None` when there is no such playlist.
    async fn playlist_songs(&self, id: &str) -> Result<Option<Vec<Song>>, Error>;

    /// The audio file of a song, served with Range support; `None` when there is no such song.
    async fn file(&self, id: &str) -> Result<Option<PathBuf>, Error>;

    /// Cover art for an id you put in [`Song::cover_art`], at roughly `size` pixels; the bytes of
    /// a JPEG or PNG. `None` means no art. The default has none.
    async fn cover(&self, _id: &str, _size: u32) -> Result<Option<Vec<u8>>, Error> {
        Ok(None)
    }

    /// A play report: `now_playing` for the "playing now" notification, otherwise a finished
    /// play that happened at `at` (Unix seconds, when the client said). `client` is the `c`
    /// parameter. The default drops reports.
    async fn scrobble(&self, _id: &str, _now_playing: bool, _at: Option<i64>, _client: &str) -> Result<(), Error> {
        Ok(())
    }

    /// Lyrics for a song, served through `getLyricsBySongId` (OpenSubsonic `songLyrics`) and
    /// `getLyrics`. The default has none.
    async fn lyrics(&self, _id: &str) -> Result<Option<Lyrics>, Error> {
        Ok(None)
    }

    /// What a client without credentials may do when the server has none configured, or
    /// when [`Server::credentials`] is set but the request carries [`Trusted`]`(false)` and no
    /// sign-in. The default allows everything.
    fn anonymous_may(&self, _scope: Scope) -> bool {
        true
    }
}
