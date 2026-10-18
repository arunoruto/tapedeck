//! The router end to end against an in-memory library: formats, sign-in, browsing, streaming
//! with Range, cover art, scrobbles.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use serde_json::Value;
use subsonic_server::{Error, Library, Playlist, Scope, Server, Song, token};
use tower::ServiceExt;

/// (song id, now playing, at, client)
type Scrobble = (String, bool, Option<i64>, String);

struct Mem {
    songs: Vec<Song>,
    file: PathBuf,
    scrobbles: Mutex<Vec<Scrobble>>,
    open: bool,
}

#[async_trait::async_trait]
impl Library for Mem {
    async fn songs(&self) -> Result<Vec<Song>, Error> {
        Ok(self.songs.clone())
    }
    async fn song(&self, id: &str) -> Result<Option<Song>, Error> {
        Ok(self.songs.iter().find(|s| s.id == id).cloned())
    }
    async fn playlists(&self) -> Result<Vec<Playlist>, Error> {
        Ok(vec![Playlist {
            id: "p1".into(),
            name: "Weekly Jams".into(),
            song_count: 2,
            duration: 8,
            ..Default::default()
        }])
    }
    async fn playlist_songs(&self, id: &str) -> Result<Option<Vec<Song>>, Error> {
        Ok((id == "p1").then(|| self.songs[..2].to_vec()))
    }
    async fn file(&self, id: &str) -> Result<Option<PathBuf>, Error> {
        Ok(self.songs.iter().any(|s| s.id == id).then(|| self.file.clone()))
    }
    async fn cover(&self, id: &str, _size: u32) -> Result<Option<Vec<u8>>, Error> {
        Ok((id == "al-a").then(|| b"\x89PNG\r\n\x1a\nfake".to_vec()))
    }
    async fn scrobble(&self, id: &str, now_playing: bool, at: Option<i64>, client: &str) -> Result<(), Error> {
        self.scrobbles.lock().unwrap().push((id.into(), now_playing, at, client.into()));
        Ok(())
    }
    fn anonymous_may(&self, scope: Scope) -> bool {
        self.open || scope == Scope::Read
    }
}

fn song(id: &str, title: &str, album_id: &str, artist: &str, no: i64) -> Song {
    Song {
        id: id.into(),
        title: title.into(),
        album: format!("Album {album_id}"),
        album_id: album_id.into(),
        artist: artist.into(),
        album_artist: artist.into(),
        artist_id: format!("ar-{artist}"),
        track: Some(no),
        duration: Some(4),
        path: format!("{artist}/{title}.flac"),
        cover_art: (album_id == "al-a").then(|| "al-a".to_string()),
        ..Default::default()
    }
}

fn mem(open: bool) -> (Arc<Mem>, PathBuf) {
    let dir = std::env::temp_dir().join(format!("subsonic-server-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("tone.flac");
    std::fs::write(&file, vec![7u8; 1000]).unwrap();
    let lib = Arc::new(Mem {
        songs: vec![
            song("1", "One", "al-a", "Zed", 1),
            song("2", "Two", "al-a", "Zed", 2),
            song("3", "Solo", "al-b", "Nine", 1),
        ],
        file: file.clone(),
        scrobbles: Mutex::new(vec![]),
        open,
    });
    (lib, file)
}

struct Shared(Arc<Mem>);

#[async_trait::async_trait]
impl Library for Shared {
    async fn songs(&self) -> Result<Vec<Song>, Error> {
        self.0.songs().await
    }
    async fn song(&self, id: &str) -> Result<Option<Song>, Error> {
        self.0.song(id).await
    }
    async fn playlists(&self) -> Result<Vec<Playlist>, Error> {
        self.0.playlists().await
    }
    async fn playlist_songs(&self, id: &str) -> Result<Option<Vec<Song>>, Error> {
        self.0.playlist_songs(id).await
    }
    async fn file(&self, id: &str) -> Result<Option<PathBuf>, Error> {
        self.0.file(id).await
    }
    async fn cover(&self, id: &str, size: u32) -> Result<Option<Vec<u8>>, Error> {
        self.0.cover(id, size).await
    }
    async fn scrobble(&self, id: &str, now_playing: bool, at: Option<i64>, client: &str) -> Result<(), Error> {
        self.0.scrobble(id, now_playing, at, client).await
    }
    fn anonymous_may(&self, scope: Scope) -> bool {
        self.0.anonymous_may(scope)
    }
}

async fn get(router: &Router, uri: &str) -> (StatusCode, String, String) {
    let res = router.clone().oneshot(Request::get(uri).body(Body::empty()).unwrap()).await.unwrap();
    let status = res.status();
    let ct = res.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let body = String::from_utf8_lossy(&res.into_body().collect().await.unwrap().to_bytes()).into_owned();
    (status, ct, body)
}

async fn json(router: &Router, uri: &str) -> Value {
    let (_, _, body) = get(router, &format!("{uri}{}f=json", if uri.contains('?') { "&" } else { "?" })).await;
    serde_json::from_str::<Value>(&body).unwrap()["subsonic-response"].clone()
}

#[tokio::test]
async fn open_server_browses_streams_and_scrobbles() {
    let (lib, _file) = mem(true);
    let router = Server::new(Shared(lib.clone())).name("test", "9.9").router();

    let (_, ct, body) = get(&router, "/rest/ping.view").await;
    assert!(ct.starts_with("text/xml"), "XML is the default: {ct}");
    assert!(
        body.contains(r#"status="ok""#) && body.contains(r#"type="test""#) && body.contains(r#"serverVersion="9.9""#),
        "{body}"
    );

    let (_, ct, body) = get(&router, "/rest/ping?f=jsonp&callback=cb").await;
    assert!(ct.starts_with("application/javascript") && body.starts_with("cb({"), "{ct} {body}");

    let artists = json(&router, "/rest/getArtists").await;
    assert_eq!(artists["artists"]["index"][0]["name"], "N");
    assert_eq!(artists["artists"]["index"][1]["artist"][0]["albumCount"], 1);

    let album = json(&router, "/rest/getAlbum?id=al-a").await;
    assert_eq!(album["album"]["song"].as_array().unwrap().len(), 2);
    assert_eq!(album["album"]["song"][0]["contentType"], "audio/flac");

    let dir = json(&router, "/rest/getMusicDirectory?id=ar-Zed").await;
    assert_eq!(dir["directory"]["child"].as_array().unwrap().len(), 1);
    let dir = json(&router, "/rest/getMusicDirectory?id=al-a").await;
    assert_eq!(dir["directory"]["child"].as_array().unwrap().len(), 2);

    let found = json(&router, "/rest/search3?query=sol").await;
    assert_eq!(found["searchResult3"]["song"][0]["title"], "Solo");

    let lists = json(&router, "/rest/getPlaylists").await;
    assert_eq!(lists["playlists"]["playlist"][0]["name"], "Weekly Jams");
    let list = json(&router, "/rest/getPlaylist?id=p1").await;
    assert_eq!(list["playlist"]["entry"].as_array().unwrap().len(), 2);

    let missing = json(&router, "/rest/getSong?id=99").await;
    assert_eq!(missing["error"]["code"], 70);
    let info = json(&router, "/rest/getArtistInfo2?id=ar-Zed").await;
    assert_eq!(info["status"], "ok");
    let unknown = json(&router, "/rest/getVideos").await;
    assert_eq!(unknown["error"]["code"], 0);

    let res = router
        .clone()
        .oneshot(Request::get("/rest/stream?id=1").header(header::RANGE, "bytes=0-99").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(res.into_body().collect().await.unwrap().to_bytes().len(), 100);

    let (status, ct, body) = get(&router, "/rest/getCoverArt?id=al-a&size=300").await;
    assert_eq!((status, ct.as_str()), (StatusCode::OK, "image/png"));
    assert!(body.ends_with("fake"));
    let none = json(&router, "/rest/getCoverArt?id=al-b").await;
    assert_eq!(none["error"]["code"], 70);

    json(&router, "/rest/scrobble?id=1&submission=false&c=cliamp").await;
    json(&router, "/rest/scrobble?id=1&id=2&time=1700000000000&time=1700000004000&c=cliamp").await;
    let got = lib.scrobbles.lock().unwrap().clone();
    assert_eq!(
        got,
        vec![
            ("1".into(), true, None, "cliamp".into()),
            ("1".into(), false, Some(1700000000), "cliamp".into()),
            ("2".into(), false, Some(1700000004), "cliamp".into()),
        ]
    );
}

#[tokio::test]
async fn credentials_and_anonymous_scopes() {
    let (lib, _file) = mem(false);
    let locked = Server::new(Shared(lib.clone())).credentials("me", "sesame").router();
    assert_eq!(json(&locked, "/rest/ping").await["error"]["code"], 10);
    assert_eq!(json(&locked, "/rest/ping?u=me&p=wrong").await["error"]["code"], 40);
    let t = token("sesame", "abc123");
    assert_eq!(json(&locked, &format!("/rest/getPlaylists?u=me&t={t}&s=abc123")).await["status"], "ok");
    assert_eq!(json(&locked, "/rest/ping?apiKey=k").await["error"]["code"], 42);

    // No credentials configured, library allows anonymous reads only.
    let open = Server::new(Shared(lib)).router();
    assert_eq!(json(&open, "/rest/getArtists?u=whoever&p=whatever").await["status"], "ok");
    assert_eq!(json(&open, "/rest/scrobble?id=1").await["error"]["code"], 50);
    let (_, ct, _) = get(&open, "/rest/stream?id=1").await;
    assert!(ct.starts_with("text/xml"), "refused as an envelope, not audio: {ct}");
}
