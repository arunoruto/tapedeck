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
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::Duration,
};

use axum::Router;
use subsonic_server::{Error, Library, Playlist, Scope, Server, Song};
use tokio::sync::mpsc;

use crate::{App, Event, app::remote_title, cli::Command, db::Track, library::is_uuid};

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
        cover_art: is_uuid(&album_id).then(|| album_id.clone()),
        music_brainz_id: is_uuid(&t.recording_mbid).then(|| t.recording_mbid.clone()),
        album_music_brainz_id: is_uuid(&album_id).then(|| album_id.clone()),
        artist_music_brainz_id: is_uuid(&t.album_artist_mbid).then(|| t.album_artist_mbid.clone()),
        album_id,
    }
}

fn internal(e: anyhow::Error) -> Error {
    Error::internal(format!("{e:#}"))
}

#[async_trait::async_trait]
impl Library for Backend {
    async fn songs(&self) -> Result<Vec<Song>, Error> {
        let tracks = self.app.db().all_tracks().map_err(internal)?;
        Ok(tracks.iter().filter(|t| playable(t)).map(song).collect())
    }

    async fn song(&self, id: &str) -> Result<Option<Song>, Error> {
        let Ok(id) = id.parse::<i64>() else { return Ok(None) };
        let track = self.app.db().track_by_id(id).map_err(internal)?;
        Ok(track.filter(playable).map(|t| song(&t)))
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
        let Ok(id) = id.parse::<i64>() else { return Ok(None) };
        let track = self.app.db().track_by_id(id).map_err(internal)?;
        // External tracks carry an absolute path; `join` keeps it.
        Ok(track.filter(playable).and_then(|t| t.path).map(|rel| self.app.config.library.join(rel)))
    }

    /// Fetched from the Cover Art Archive once per size and kept in `covers/` next to the
    /// database; an empty file remembers "none", so the archive is asked once.
    async fn cover(&self, id: &str, size: u32) -> Result<Option<Vec<u8>>, Error> {
        let mbid = match id.parse::<i64>() {
            Ok(n) => self.app.db().track_by_id(n).map_err(internal)?.map(|t| t.album_mbid).unwrap_or_default(),
            Err(_) => id.to_string(),
        };
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
    async fn scrobble(&self, id: &str, now_playing: bool, _at: Option<i64>, client: &str) -> Result<(), Error> {
        let track = id.parse::<i64>().map_err(|_| Error::not_found("no such song"))?;
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

    fn anonymous_may(&self, scope: Scope) -> bool {
        let name = match scope {
            Scope::Read => "read",
            Scope::Stream => "stream",
            Scope::Listen => "listen",
        };
        self.anonymous.iter().any(|a| a == name)
    }
}

static HTTP: OnceLock<reqwest::Client> = OnceLock::new();

fn http() -> &'static reqwest::Client {
    HTTP.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(crate::USER_AGENT)
            .timeout(Duration::from_secs(20))
            .build()
            .expect("reqwest client")
    })
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
