//! The router and the method dispatch.

use std::{
    hash::{BuildHasher, RandomState},
    sync::Arc,
};

use axum::{
    Router,
    extract::{Path, Request, State},
    http::header,
    response::{IntoResponse, Response},
    routing::any,
};
use serde_json::{Value, json};
use tower::ServiceExt;
use tower_http::services::ServeFile;

use crate::{
    Error, Library, Scope,
    auth::{Credentials, authorize},
    render::{Format, Identity, envelope},
    shelf::{Shelf, album_json, artist_json, index, playlist_json, song_json},
};

/// Request extension your own middleware may insert: `Trusted(true)` lets the request through
/// as signed in, whatever its Subsonic parameters say.
#[derive(Clone, Copy, Debug)]
pub struct Trusted(pub bool);

/// Builder for the router; see the crate docs.
pub struct Server {
    lib: Arc<dyn Library>,
    creds: Option<Credentials>,
    name: String,
    version: String,
}

/// Query and form parameters, repeats kept (`scrobble` sends several `id`s).
type Params = Vec<(String, String)>;

fn one<'a>(p: &'a Params, k: &str) -> Option<&'a str> {
    p.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
}

fn num<T: std::str::FromStr>(p: &Params, k: &str, default: T) -> T {
    one(p, k).and_then(|v| v.parse().ok()).unwrap_or(default)
}

impl Server {
    pub fn new(lib: impl Library) -> Self {
        Self {
            lib: Arc::new(lib),
            creds: None,
            name: "subsonic-server".into(),
            version: env!("CARGO_PKG_VERSION").into(),
        }
    }

    /// What `type` and `serverVersion` in every response say.
    pub fn name(mut self, name: &str, version: &str) -> Self {
        self.name = name.into();
        self.version = version.into();
        self
    }

    /// Require this sign-in. Without it every client is anonymous and
    /// [`Library::anonymous_may`] decides what it can do.
    pub fn credentials(mut self, user: &str, password: &str) -> Self {
        self.creds = Some(Credentials { user: user.into(), password: password.into() });
        self
    }

    /// Serves `/rest/{method}` and `/rest/{method}.view`, GET or form POST.
    pub fn router(self) -> Router {
        Router::new().route("/rest/{method}", any(rest)).with_state(Arc::new(self))
    }
}

async fn rest(State(s): State<Arc<Server>>, Path(method): Path<String>, req: Request) -> Response {
    let method = method.trim_end_matches(".view").to_string();
    let trusted = req.extensions().get::<Trusted>().is_some_and(|t| t.0);
    let mut params: Params = serde_urlencoded::from_str(req.uri().query().unwrap_or("")).unwrap_or_default();
    let fmt = Format::from_params(one(&params, "f"), one(&params, "callback"));
    let streaming = matches!(method.as_str(), "stream" | "download");
    let req = if streaming {
        Some(req)
    } else {
        let form = req
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|c| c.starts_with("application/x-www-form-urlencoded"));
        if form && let Ok(bytes) = axum::body::to_bytes(req.into_body(), 64 * 1024).await {
            params.extend(serde_urlencoded::from_bytes::<Params>(&bytes).unwrap_or_default());
        }
        None
    };
    tracing::debug!(
        "subsonic: {method} {:?}",
        params.iter().filter(|(k, _)| !matches!(k.as_str(), "t" | "s" | "p")).collect::<Vec<_>>()
    );
    let who = Identity { name: &s.name, version: &s.version };
    match handle(&s, trusted, &method, &params, req).await {
        Ok(Reply::Json(v)) => envelope(&fmt, &who, Ok(v)),
        Ok(Reply::Raw(r)) => r,
        Err(e) => envelope(&fmt, &who, Err(e)),
    }
}

enum Reply {
    Json(Value),
    Raw(Response),
}

async fn handle(s: &Server, trusted: bool, method: &str, p: &Params, req: Option<Request>) -> Result<Reply, Error> {
    let signed_in =
        authorize(s.creds.as_ref(), trusted, one(p, "u"), one(p, "t"), one(p, "s"), one(p, "p"), one(p, "apiKey"))?;
    let scope = match method {
        "stream" | "download" | "getCoverArt" => Scope::Stream,
        "scrobble" => Scope::Listen,
        _ => Scope::Read,
    };
    if !signed_in && !s.lib.anonymous_may(scope) {
        return Err(Error::new(50, format!("`{method}` is not open to anonymous clients; sign in")));
    }
    let who = s.creds.as_ref().map(|c| c.user.as_str()).or(one(p, "u")).unwrap_or("anonymous");
    let lib = &s.lib;
    let shelf = || async { Ok::<_, Error>(Shelf { songs: lib.songs().await? }) };
    let id = || one(p, "id").ok_or_else(|| Error::missing("id"));
    let v = match method {
        "ping" => json!({}),
        "getLicense" => json!({ "license": { "valid": true } }),
        "getOpenSubsonicExtensions" => json!({ "openSubsonicExtensions": [{ "name": "songLyrics", "versions": [1] }] }),
        "getMusicFolders" => json!({ "musicFolders": { "musicFolder": [{ "id": 1, "name": s.name }] } }),
        "getUser" => json!({ "user": {
            "username": who, "scrobblingEnabled": true, "adminRole": false, "settingsRole": false,
            "downloadRole": true, "uploadRole": false, "playlistRole": false, "coverArtRole": false,
            "commentRole": false, "podcastRole": false, "streamRole": true, "jukeboxRole": false,
            "shareRole": false, "videoConversionRole": false, "folder": [1],
        } }),
        "getArtists" | "getIndexes" => {
            let key = if method == "getArtists" { "artists" } else { "indexes" };
            let shelf = shelf().await?;
            json!({ key: { "ignoredArticles": "", "index": index(&shelf.artists()) } })
        }
        "getArtist" => {
            let id = id()?;
            let shelf = shelf().await?;
            let artist =
                shelf.artists().into_iter().find(|a| a.id == id).ok_or_else(|| Error::not_found("no such artist"))?;
            let mut v = artist_json(&artist);
            v["album"] = json!(artist.albums.iter().map(album_json).collect::<Vec<_>>());
            json!({ "artist": v })
        }
        "getAlbum" => {
            let id = id()?;
            let shelf = shelf().await?;
            let album =
                shelf.albums().into_iter().find(|a| a.id == id).ok_or_else(|| Error::not_found("no such album"))?;
            let mut v = album_json(&album);
            v["song"] = json!(album.songs.iter().map(|s| song_json(s)).collect::<Vec<_>>());
            json!({ "album": v })
        }
        "getMusicDirectory" => {
            let id = id()?;
            let shelf = shelf().await?;
            if let Some(artist) = shelf.artists().into_iter().find(|a| a.id == id) {
                json!({ "directory": { "id": artist.id, "name": artist.name,
                    "child": artist.albums.iter().map(album_json).collect::<Vec<_>>() } })
            } else if let Some(album) = shelf.albums().into_iter().find(|a| a.id == id) {
                json!({ "directory": { "id": album.id, "parent": album.artist_id, "name": album.name,
                    "child": album.songs.iter().map(|s| song_json(s)).collect::<Vec<_>>() } })
            } else {
                return Err(Error::not_found("no such directory"));
            }
        }
        "getSong" => {
            let song = lib.song(id()?).await?.ok_or_else(|| Error::not_found("no such song"))?;
            json!({ "song": song_json(&song) })
        }
        "getAlbumList" | "getAlbumList2" => {
            let key = if method == "getAlbumList2" { "albumList2" } else { "albumList" };
            let shelf = shelf().await?;
            let mut albums = shelf.albums();
            match one(p, "type").unwrap_or("alphabeticalByName") {
                "alphabeticalByArtist" => albums.sort_by_key(|a| (a.artist.to_lowercase(), a.name.to_lowercase())),
                "byYear" => albums.sort_by_key(|a| a.year),
                // No play counts or added dates per album; the newest songs stand in for all three.
                "newest" | "recent" | "frequent" => albums.sort_by_key(|a| std::cmp::Reverse(a.newest)),
                "random" => {
                    let rs = RandomState::new();
                    albums.sort_by_key(|a| rs.hash_one(a.id));
                }
                _ => {}
            }
            let (size, offset) = (num::<usize>(p, "size", 10).min(500), num::<usize>(p, "offset", 0));
            json!({ key: { "album": albums.iter().skip(offset).take(size).map(album_json).collect::<Vec<_>>() } })
        }
        "search2" | "search3" => {
            let key = if method == "search3" { "searchResult3" } else { "searchResult2" };
            // Some clients send `""` to mean "everything".
            let q = one(p, "query").unwrap_or("").trim().trim_matches('"').to_lowercase();
            let hit = |s: &str| q.is_empty() || s.to_lowercase().contains(&q);
            let shelf = shelf().await?;
            let page = |what: &str| {
                (num::<usize>(p, &format!("{what}Offset"), 0), num::<usize>(p, &format!("{what}Count"), 20))
            };
            let ((ao, ac), (lo, lc), (so, sc)) = (page("artist"), page("album"), page("song"));
            json!({ key: {
                "artist": shelf.artists().iter().filter(|a| hit(a.name)).skip(ao).take(ac).map(artist_json).collect::<Vec<_>>(),
                "album": shelf.albums().iter().filter(|a| hit(a.name) || hit(a.artist)).skip(lo).take(lc).map(album_json).collect::<Vec<_>>(),
                "song": shelf.songs.iter().filter(|s| hit(&s.title) || hit(&s.artist) || hit(&s.album)).skip(so).take(sc).map(song_json).collect::<Vec<_>>(),
            } })
        }
        "getRandomSongs" => {
            let rs = RandomState::new();
            let mut songs = lib.songs().await?;
            songs.sort_by_key(|s| rs.hash_one(&s.id));
            let size = num::<usize>(p, "size", 10).min(500);
            json!({ "randomSongs": { "song": songs.iter().take(size).map(song_json).collect::<Vec<_>>() } })
        }
        "getPlaylists" => {
            let lists = lib.playlists().await?;
            json!({ "playlists": { "playlist": lists.iter().map(|pl| playlist_json(pl, who)).collect::<Vec<_>>() } })
        }
        "getPlaylist" => {
            let id = id()?;
            let lists = lib.playlists().await?;
            let pl = lists.iter().find(|pl| pl.id == id).ok_or_else(|| Error::not_found("no such playlist"))?;
            let songs = lib.playlist_songs(id).await?.unwrap_or_default();
            let mut v = playlist_json(pl, who);
            v["entry"] = json!(songs.iter().map(song_json).collect::<Vec<_>>());
            json!({ "playlist": v })
        }
        "getScanStatus" => json!({ "scanStatus": { "scanning": false, "count": lib.songs().await?.len() } }),
        "stream" | "download" => {
            return Ok(Reply::Raw(stream(s, p, req.expect("request kept for streaming")).await));
        }
        "getCoverArt" => {
            let id = id()?;
            let size = num::<u32>(p, "size", 500);
            let bytes = lib.cover(id, size).await?.ok_or_else(|| Error::not_found("no cover art for this item"))?;
            let content_type = if bytes.starts_with(b"\x89PNG") { "image/png" } else { "image/jpeg" };
            let headers = [(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, "public, max-age=604800")];
            return Ok(Reply::Raw((headers, bytes).into_response()));
        }
        "scrobble" => {
            let now_playing = one(p, "submission").is_some_and(|v| v == "false");
            let ids: Vec<&str> = p.iter().filter(|(k, _)| k == "id").map(|(_, v)| v.as_str()).collect();
            if ids.is_empty() {
                return Err(Error::missing("id"));
            }
            let times: Vec<Option<i64>> =
                p.iter().filter(|(k, _)| k == "time").map(|(_, v)| v.parse::<i64>().ok().map(|ms| ms / 1000)).collect();
            let client = one(p, "c").unwrap_or("?");
            for (i, id) in ids.iter().enumerate() {
                lib.scrobble(id, now_playing, times.get(i).copied().flatten(), client).await?;
            }
            json!({})
        }
        // Accepted so clients' favourite buttons do not error; nothing is kept.
        "star" | "unstar" | "setRating" => json!({}),
        "getStarred" | "getStarred2" => {
            let key = if method == "getStarred2" { "starred2" } else { "starred" };
            json!({ key: { "artist": [], "album": [], "song": [] } })
        }
        "getNowPlaying" => json!({ "nowPlaying": { "entry": [] } }),
        "getGenres" => json!({ "genres": { "genre": [] } }),
        // Nothing to say, but a valid answer: clients ask for these right after login and show
        // the server as broken on a failure.
        "getArtistInfo" => json!({ "artistInfo": {} }),
        "getArtistInfo2" => json!({ "artistInfo2": {} }),
        "getAlbumInfo" | "getAlbumInfo2" => json!({ "albumInfo": {} }),
        "getTopSongs" => json!({ "topSongs": {} }),
        "getSimilarSongs" => json!({ "similarSongs": {} }),
        "getSimilarSongs2" => json!({ "similarSongs2": {} }),
        "getLyricsBySongId" => {
            let id = id()?;
            let song = lib.song(id).await?.ok_or_else(|| Error::not_found("no such song"))?;
            let structured = match lib.lyrics(id).await? {
                Some(l) => structured_lyrics(&song, &l),
                None => vec![],
            };
            json!({ "lyricsList": { "structuredLyrics": structured } })
        }
        "getLyrics" => {
            let (artist, title) = (one(p, "artist").unwrap_or(""), one(p, "title").unwrap_or(""));
            let songs = lib.songs().await?;
            let hit = songs.iter().find(|s| {
                s.title.eq_ignore_ascii_case(title) && (artist.is_empty() || s.artist.eq_ignore_ascii_case(artist))
            });
            match hit {
                Some(s) => match lib.lyrics(&s.id).await? {
                    Some(l) => json!({ "lyrics": { "artist": s.artist, "title": s.title, "value": plain_text(&l) } }),
                    None => json!({ "lyrics": {} }),
                },
                None => json!({ "lyrics": {} }),
            }
        }
        "getInternetRadioStations" => json!({ "internetRadioStations": {} }),
        "getPodcasts" => json!({ "podcasts": {} }),
        "getNewestPodcasts" => json!({ "newestPodcasts": {} }),
        "getShares" => json!({ "shares": {} }),
        "getBookmarks" => json!({ "bookmarks": {} }),
        "getPlayQueue" | "savePlayQueue" | "startScan" => json!({}),
        _ => return Err(Error::new(0, format!("{} does not implement {method}", s.name))),
    };
    Ok(Reply::Json(v))
}

/// The OpenSubsonic `structuredLyrics` entries: a synced one from the LRC text, a plain one
/// from the plain text.
fn structured_lyrics(song: &crate::Song, l: &crate::Lyrics) -> Vec<Value> {
    let base = |synced: bool, lines: Vec<Value>| {
        json!({
            "lang": "xxx",
            "synced": synced,
            "displayArtist": song.artist,
            "displayTitle": song.title,
            "offset": 0,
            "line": lines,
        })
    };
    let mut out = vec![];
    if let Some(lrc) = &l.synced {
        let lines: Vec<Value> =
            parse_lrc(lrc).into_iter().map(|(ms, text)| json!({ "start": ms, "value": text })).collect();
        if !lines.is_empty() {
            out.push(base(true, lines));
        }
    }
    if let Some(plain) = &l.plain {
        out.push(base(false, plain.lines().map(|t| json!({ "value": t })).collect()));
    }
    out
}

fn plain_text(l: &crate::Lyrics) -> String {
    match (&l.plain, &l.synced) {
        (Some(p), _) => p.clone(),
        (None, Some(s)) => parse_lrc(s).into_iter().map(|(_, t)| t).collect::<Vec<_>>().join("\n"),
        (None, None) => String::new(),
    }
}

/// `[mm:ss.xx]text` lines (several stamps per line allowed) → (milliseconds, text), in time order.
pub fn parse_lrc(lrc: &str) -> Vec<(u64, String)> {
    let mut out = vec![];
    for line in lrc.lines() {
        let mut rest = line.trim_start();
        let mut stamps = vec![];
        // `rest` starts with '[' and the ']' sits at `close`.
        while let Some(close) = rest.strip_prefix('[').and_then(|r| r.find(']').map(|e| e + 1)) {
            match stamp_ms(&rest[1..close]) {
                Some(ms) => stamps.push(ms),
                None => break, // a tag like [ar:…]
            }
            rest = rest[close + 1..].trim_start();
        }
        for ms in stamps {
            out.push((ms, rest.trim().to_string()));
        }
    }
    out.sort_by_key(|(ms, _)| *ms);
    out
}

fn stamp_ms(s: &str) -> Option<u64> {
    let (m, rest) = s.split_once(':')?;
    let m: u64 = m.trim().parse().ok()?;
    let secs: f64 = rest.trim().parse().ok()?;
    Some(m * 60_000 + (secs * 1000.0).round() as u64)
}

/// The song's file with Range support; errors go out as the envelope the client asked for.
async fn stream(s: &Server, p: &Params, req: Request) -> Response {
    let fmt = Format::from_params(one(p, "f"), one(p, "callback"));
    let who = Identity { name: &s.name, version: &s.version };
    let file = match one(p, "id") {
        Some(id) => s.lib.file(id).await,
        None => Err(Error::missing("id")),
    };
    let file = match file {
        Ok(Some(f)) => f,
        Ok(None) => return envelope(&fmt, &who, Err(Error::not_found("no such song"))),
        Err(e) => return envelope(&fmt, &who, Err(e)),
    };
    match ServeFile::new(&file).oneshot(req).await {
        Ok(res) => res.into_response(),
        Err(e) => envelope(&fmt, &who, Err(Error::internal(e))),
    }
}
