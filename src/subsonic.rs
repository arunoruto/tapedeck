//! tapedeck as a Subsonic server: the `Library` the subsonic-server crate (crates/) needs,
//! backed by the database and the library folder, mounted on the `[server] listen` port under
//! `/rest/`. The protocol lives in the crate; this file only says where tapedeck's songs,
//! playlists, files, covers and scrobbles come from.
//!
//! Ids: songs are track ids, albums and artists their MusicBrainz ids (so names follow
//! MusicBrainz and ids survive renames), with the name as a fallback for the odd video-only
//! row. Covers come from the Cover Art Archive by release id, fetched once per size and kept
//! in `covers/` next to the database. Scrobbles go through `td listen`, so the daemon's
//! ListenBrainz token and retry queue apply. Sign-in is `[server.subsonic]`; without it every
//! client is anonymous and gets the `[server] anonymous` scopes.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use axum::Router;
use subsonic_server::{Error, Library, Lyrics, Playlist, Scope, Server, Song};
use tokio::sync::mpsc;

use crate::{
    App, Event,
    app::remote_title,
    cli::Command,
    db::{ExternalFile, Track},
    http,
    library::is_uuid,
    steam::is_steam_id,
};

/// The `/rest/*` router for this app, with `[server.subsonic]` credentials when configured.
pub fn router(app: Arc<App>, anonymous: Vec<String>) -> Router {
    let sub = &app.config.server.subsonic;
    let creds = sub.user.clone().zip(sub.password.clone());
    let mut server = Server::new(Backend { app: app.clone(), anonymous }).name("tapedeck", env!("CARGO_PKG_VERSION"));
    if let Some((user, password)) = creds {
        server = server.credentials(&user, &password);
    }
    server.router()
}

struct Backend {
    app: Arc<App>,
    anonymous: Vec<String>,
}

fn playable(t: &Track) -> bool {
    t.path.is_some() && matches!(t.status.as_str(), "ready" | "external")
}

fn song(t: &Track) -> Song {
    let album_id =
        if t.album_mbid.is_empty() { format!("{}\u{1f}{}", t.album_artist, t.album) } else { t.album_mbid.clone() };
    Song {
        id: t.id.to_string(),
        title: t.title.clone(),
        artist: t.artist.clone(),
        album_artist: t.album_artist.clone(),
        album: t.album.clone(),
        artist_id: if t.album_artist_mbid.is_empty() { t.album_artist.clone() } else { t.album_artist_mbid.clone() },
        track: t.track_no,
        year: t.year,
        duration: t.duration_ms.map(|ms| ms / 1000),
        path: t.path.clone().unwrap_or_default(),
        cover_art: (is_uuid(&album_id) || is_steam_id(&album_id)).then(|| album_id.clone()),
        music_brainz_id: is_uuid(&t.recording_mbid).then(|| t.recording_mbid.clone()),
        album_music_brainz_id: is_uuid(&album_id).then(|| album_id.clone()),
        artist_music_brainz_id: is_uuid(&t.album_artist_mbid).then(|| t.album_artist_mbid.clone()),
        album_id,
    }
}

fn internal(e: anyhow::Error) -> Error {
    Error::internal(format!("{e:#}"))
}

/// A stable id for a string that is not a number: FNV-1a, hex.
fn fnv(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// Files of external libraries (Lidarr's) that no track stands for, as songs: ids are
/// `x<hash of path>`, albums `xa<hash>`, artists `xr<hash>`, all derived from the tags.
fn external_song(e: &ExternalFile) -> Song {
    let album_artist = e.album_artist.clone().or_else(|| Some(e.artist.clone())).unwrap_or_default();
    let album = e.album.clone().unwrap_or_else(|| "Unknown album".into());
    let album_id = format!("xa{}", fnv(&format!("{album_artist}\u{1f}{album}")));
    let rel =
        Path::new(&e.path).strip_prefix(&e.root).map(|p| p.to_string_lossy().into_owned()).unwrap_or(e.path.clone());
    Song {
        id: format!("x{}", fnv(&e.path)),
        title: e.title.clone(),
        artist: e.artist.clone(),
        album_artist: album_artist.clone(),
        album,
        artist_id: format!("xr{}", fnv(&album_artist)),
        track: e.track_no,
        year: e.year,
        duration: e.duration_ms.map(|ms| ms / 1000),
        path: rel,
        cover_art: Some(album_id.clone()),
        music_brainz_id: e.recording_mbid.clone().filter(|m| is_uuid(m)),
        album_music_brainz_id: None,
        artist_music_brainz_id: None,
        album_id,
    }
}

impl Backend {
    /// External files not already represented by a track.
    fn externals(&self) -> Result<Vec<ExternalFile>, Error> {
        let tracks = self.app.db().all_tracks().map_err(internal)?;
        let taken: std::collections::HashSet<&str> =
            tracks.iter().filter(|t| t.status == "external").filter_map(|t| t.path.as_deref()).collect();
        let all = self.app.db().external_all().map_err(internal)?;
        Ok(all.into_iter().filter(|e| !taken.contains(e.path.as_str())).collect())
    }

    fn external_by_song_id(&self, id: &str) -> Result<Option<ExternalFile>, Error> {
        let Some(hash) = id.strip_prefix('x').filter(|h| h.len() == 16) else { return Ok(None) };
        Ok(self.externals()?.into_iter().find(|e| fnv(&e.path) == hash))
    }
}

#[async_trait::async_trait]
impl Library for Backend {
    async fn songs(&self) -> Result<Vec<Song>, Error> {
        let tracks = self.app.db().all_tracks().map_err(internal)?;
        let mut songs: Vec<Song> = tracks.iter().filter(|t| playable(t)).map(song).collect();
        songs.extend(self.externals()?.iter().map(external_song));
        Ok(songs)
    }

    async fn song(&self, id: &str) -> Result<Option<Song>, Error> {
        if let Ok(id) = id.parse::<i64>() {
            let track = self.app.db().track_by_id(id).map_err(internal)?;
            return Ok(track.filter(playable).map(|t| song(&t)));
        }
        Ok(self.external_by_song_id(id)?.map(|e| external_song(&e)))
    }

    async fn playlists(&self) -> Result<Vec<Playlist>, Error> {
        let all = self.app.db().playlists().map_err(internal)?;
        let mut out = Vec::with_capacity(all.len());
        for pl in &all {
            let tracks = self.app.db().playlist_tracks(pl.id).map_err(internal)?;
            let songs: Vec<Song> = tracks.iter().filter(|t| playable(t)).map(song).collect();
            out.push(Playlist {
                id: pl.id.to_string(),
                name: remote_title(&self.app.config, pl, &all),
                comment: Some(pl.name.clone()),
                owner: None,
                created: Some(pl.fetched_at.clone()),
                changed: Some(pl.fetched_at.clone()),
                song_count: songs.len() as i64,
                duration: songs.iter().map(|s| s.duration.unwrap_or(0)).sum(),
                cover_art: songs.iter().find_map(|s| s.cover_art.clone()),
            });
        }
        Ok(out)
    }

    async fn playlist_songs(&self, id: &str) -> Result<Option<Vec<Song>>, Error> {
        let Ok(id) = id.parse::<i64>() else { return Ok(None) };
        let known = self.app.db().playlists().map_err(internal)?.iter().any(|p| p.id == id);
        if !known {
            return Ok(None);
        }
        let tracks = self.app.db().playlist_tracks(id).map_err(internal)?;
        Ok(Some(tracks.iter().filter(|t| playable(t)).map(song).collect()))
    }

    async fn file(&self, id: &str) -> Result<Option<PathBuf>, Error> {
        let Ok(id) = id.parse::<i64>() else {
            return Ok(self.external_by_song_id(id)?.map(|e| PathBuf::from(e.path)));
        };
        let track = self.app.db().track_by_id(id).map_err(internal)?;
        // External tracks carry an absolute path; `join` keeps it.
        Ok(track.filter(playable).and_then(|t| t.path).map(|rel| self.app.config.library.join(rel)))
    }

    /// MusicBrainz releases: fetched from the Cover Art Archive once per size and kept in
    /// `covers/` next to the database (an empty file remembers "none", so the archive is asked
    /// once). Steam soundtracks: the picture embedded in the first file, or a cover.jpg beside it.
    async fn cover(&self, id: &str, size: u32) -> Result<Option<Vec<u8>>, Error> {
        let mbid = match id.parse::<i64>() {
            Ok(n) => self.app.db().track_by_id(n).map_err(internal)?.map(|t| t.album_mbid).unwrap_or_default(),
            Err(_) => id.to_string(),
        };
        if is_steam_id(&mbid) || mbid.starts_with("xa") {
            // Steam albums and external albums: the picture in the first file, or the folder's.
            let file = if is_steam_id(&mbid) {
                let tracks = self.app.db().all_tracks().map_err(internal)?;
                let rel = tracks.iter().find(|t| t.album_mbid == mbid && playable(t)).and_then(|t| t.path.clone());
                rel.map(|rel| self.app.config.library.join(rel))
            } else {
                self.externals()?
                    .iter()
                    .map(external_song)
                    .zip(self.externals()?)
                    .find(|(s, _)| s.album_id == mbid)
                    .map(|(_, e)| PathBuf::from(e.path))
            };
            return tokio::task::spawn_blocking(move || file.and_then(|f| crate::steam::cover_for(&f)))
                .await
                .map_err(Error::internal);
        }
        if !is_uuid(&mbid) {
            return Ok(None);
        }
        let size = match size {
            s if s <= 250 => 250,
            s if s <= 500 => 500,
            _ => 1200,
        };
        let dir = self
            .app
            .config
            .database_path()
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_default()
            .join("covers");
        let file = dir.join(format!("{mbid}-{size}"));
        let bytes = match tokio::fs::read(&file).await {
            Ok(b) => b,
            Err(_) => {
                let url = format!("https://coverartarchive.org/release/{mbid}/front-{size}");
                let res =
                    http().get(&url).send().await.map_err(|e| Error::internal(format!("cover art archive: {e}")))?;
                let bytes = match res.status() {
                    st if st.is_success() => res.bytes().await.map_err(Error::internal)?.to_vec(),
                    reqwest::StatusCode::NOT_FOUND => vec![],
                    st => return Err(Error::internal(format!("cover art archive: HTTP {st}"))),
                };
                let _ = tokio::fs::create_dir_all(&dir).await;
                let _ = tokio::fs::write(&file, &bytes).await;
                bytes
            }
        };
        Ok((!bytes.is_empty()).then_some(bytes))
    }

    /// Through `td listen`, so the daemon's token check and queue apply.
    // ponytail: the client's timestamp is dropped, the listen is stamped on arrival; thread it
    // through Command::Listen when a client that submits backlogs shows up.
    async fn scrobble(&self, id: &str, now_playing: bool, at: Option<i64>, client: &str) -> Result<(), Error> {
        let Ok(track) = id.parse::<i64>() else {
            // An external file: no track row, so straight to ListenBrainz.
            let e = self.external_by_song_id(id)?.ok_or_else(|| Error::not_found("no such song"))?;
            tracing::info!(
                "subsonic: {} external {} - {} from {client}",
                if now_playing { "playing now" } else { "scrobble" },
                e.artist,
                e.title
            );
            let listened_at = if now_playing {
                None
            } else {
                Some(at.unwrap_or_else(|| {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0)
                }))
            };
            return self.app.listen_external(&e, listened_at).await.map_err(internal);
        };
        tracing::info!(
            "subsonic: {} track {track} from {client}",
            if now_playing { "playing now" } else { "scrobble" }
        );
        let (tx, mut rx) = mpsc::channel(16);
        let app = self.app.clone();
        let run = tokio::spawn(async move { app.run(Command::Listen { track, now: now_playing }, tx).await });
        while let Some(ev) = rx.recv().await {
            if let Event::Error { msg } = ev {
                return Err(Error::internal(msg));
            }
        }
        let _ = run.await;
        Ok(())
    }

    async fn lyrics(&self, id: &str) -> Result<Option<Lyrics>, Error> {
        let key = match id.parse::<i64>() {
            Ok(n) => format!("t:{n}"),
            Err(_) => match self.external_by_song_id(id)? {
                Some(e) => format!("x:{}", e.path),
                None => return Ok(None),
            },
        };
        let row = self.app.db().lyrics_get(&key).map_err(internal)?;
        Ok(row.filter(|r| r.status == "found").map(|r| Lyrics { synced: r.synced, plain: r.plain }))
    }

    fn anonymous_may(&self, scope: Scope) -> bool {
        let name = match scope {
            Scope::Read => "read",
            Scope::Stream => "stream",
            Scope::Listen => "listen",
        };
        self.anonymous.iter().any(|a| a == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_mbids_with_name_fallbacks() {
        let t = Track {
            id: 7,
            title: "One".into(),
            album: "Album A".into(),
            album_mbid: "11111111-1111-1111-1111-111111111111".into(),
            artist: "Zed".into(),
            album_artist: "Zed".into(),
            album_artist_mbid: "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".into(),
            recording_mbid: "youtube:abc".into(),
            path: Some("Zed/Album A/01 - One.opus".into()),
            status: "ready".into(),
            duration_ms: Some(61_000),
            ..Default::default()
        };
        let s = song(&t);
        assert_eq!(
            (s.id.as_str(), s.album_id.as_str(), s.artist_id.as_str()),
            ("7", &t.album_mbid[..], &t.album_artist_mbid[..])
        );
        assert_eq!(s.cover_art.as_deref(), Some(&t.album_mbid[..]));
        assert_eq!(s.music_brainz_id, None, "a youtube pseudo-id is not advertised as an MBID");
        assert_eq!(s.duration, Some(61));

        let loose = Track {
            album_mbid: String::new(),
            album_artist_mbid: String::new(),
            album_artist: "Nine".into(),
            album: "Loose".into(),
            ..t
        };
        let s = song(&loose);
        assert_eq!(s.artist_id, "Nine");
        assert_eq!(s.album_id, "Nine\u{1f}Loose");
        assert_eq!(s.cover_art, None);
    }
}
