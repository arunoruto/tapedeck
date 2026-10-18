//! A Subsonic face for the daemon, so cliamp, Symfonium, Feishin, DSub and the other Subsonic
//! apps play the library straight from tapedeck and scrobble through it. It shares the
//! `[server] listen` port under `/rest/`, answers JSON whatever `f` says, and covers the calls a
//! player needs: ping, browsing by artist/album (ID3 style), search, playlists, stream, cover
//! art, scrobble. The rest returns a Subsonic error rather than a 404.
//!
//! Sign-in is the protocol's own `u`+`t`+`s` (or `p`) check against `[server.subsonic]`. Without
//! a user configured, every client is anonymous and gets the `[server] anonymous` scopes, which
//! is all a LAN needs; with one, a signed-in client may do everything.

use std::{
    collections::BTreeMap,
    hash::{BuildHasher, RandomState},
    sync::Arc,
};

use axum::{
    extract::{Path, Request, State},
    http::header,
    response::{IntoResponse, Redirect, Response},
};
use md5::{Digest, Md5};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tower::ServiceExt;
use tower_http::services::ServeFile;

use crate::{
    Event,
    api::{Admin, Shared, allowed},
    app::remote_title,
    cli::Command,
    config::Subsonic,
    db::Track,
};

const API_VERSION: &str = "1.16.1";

/// Query and form parameters, repeats kept (`scrobble` sends several `id`s).
type Params = Vec<(String, String)>;

/// A Subsonic error: HTTP 200, `status: failed`, one of the protocol's codes.
#[derive(Debug)]
pub struct Fail {
    code: u16,
    msg: String,
}

fn fail(code: u16, msg: impl Into<String>) -> Fail {
    Fail { code, msg: msg.into() }
}

impl From<anyhow::Error> for Fail {
    fn from(e: anyhow::Error) -> Self {
        fail(0, format!("{e:#}"))
    }
}

fn one<'a>(p: &'a Params, k: &str) -> Option<&'a str> {
    p.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
}

fn num<T: std::str::FromStr>(p: &Params, k: &str, default: T) -> T {
    one(p, k).and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// `/rest/{method}` and `/rest/{method}.view`, GET or form POST.
pub(crate) async fn rest(State(s): State<Arc<Shared>>, Path(method): Path<String>, req: Request) -> Response {
    let method = method.trim_end_matches(".view").to_string();
    let admin = req.extensions().get::<Admin>().is_some_and(|a| a.0);
    let mut params: Params = serde_urlencoded::from_str(req.uri().query().unwrap_or("")).unwrap_or_default();
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
    match handle(&s, admin, &method, &params, req).await {
        Ok(Reply::Json(v)) => envelope(json!({ "status": "ok" }), v),
        Ok(Reply::Raw(r)) => r,
        Err(e) => envelope(json!({ "status": "failed", "error": { "code": e.code, "message": e.msg } }), json!({})),
    }
}

enum Reply {
    Json(Value),
    Raw(Response),
}

fn envelope(mut base: Value, data: Value) -> Response {
    let obj = base.as_object_mut().expect("object");
    obj.insert("version".into(), json!(API_VERSION));
    obj.insert("type".into(), json!("tapedeck"));
    obj.insert("serverVersion".into(), json!(env!("CARGO_PKG_VERSION")));
    obj.insert("openSubsonic".into(), json!(true));
    if let Value::Object(m) = data {
        obj.extend(m);
    }
    let mut body = json!({ "subsonic-response": base });
    strip_nulls(&mut body);
    ([(header::CONTENT_TYPE, "application/json")], body.to_string()).into_response()
}

/// Some Android clients crash on a key that is present but null; leave optional fields out.
fn strip_nulls(v: &mut Value) {
    match v {
        Value::Object(m) => {
            m.retain(|_, v| !v.is_null());
            m.values_mut().for_each(strip_nulls);
        }
        Value::Array(a) => a.iter_mut().for_each(strip_nulls),
        _ => {}
    }
}

async fn handle(s: &Shared, bearer_admin: bool, method: &str, p: &Params, req: Option<Request>) -> Result<Reply, Fail> {
    let cfg = &s.app.config;
    let admin = authorize(&cfg.server.subsonic, bearer_admin, p)?;
    let scope = match method {
        "stream" | "download" | "getCoverArt" => "stream",
        "scrobble" => "listen",
        _ => "read",
    };
    if !allowed(s, admin, scope) {
        return Err(fail(
            50,
            format!(
                "`{method}` needs the {scope} scope: add it to [server] anonymous or set [server.subsonic] user and password"
            ),
        ));
    }
    let who = cfg.server.subsonic.user.as_deref().or(one(p, "u")).unwrap_or("anonymous");
    let v = match method {
        "ping" => json!({}),
        "getLicense" => json!({ "license": { "valid": true } }),
        "getOpenSubsonicExtensions" => json!({ "openSubsonicExtensions": [] }),
        "getMusicFolders" => json!({ "musicFolders": { "musicFolder": [{ "id": 1, "name": "tapedeck" }] } }),
        "getUser" => json!({ "user": {
            "username": who, "scrobblingEnabled": true, "adminRole": false, "settingsRole": false,
            "downloadRole": true, "uploadRole": false, "playlistRole": false, "coverArtRole": false,
            "commentRole": false, "podcastRole": false, "streamRole": true, "jukeboxRole": false,
            "shareRole": false, "videoConversionRole": false, "folder": [1],
        } }),
        "getArtists" | "getIndexes" => {
            let key = if method == "getArtists" { "artists" } else { "indexes" };
            let shelf = Shelf::load(s)?;
            json!({ key: { "ignoredArticles": "", "index": index(&shelf.artists()) } })
        }
        "getArtist" => {
            let id = one(p, "id").ok_or_else(|| fail(10, "missing parameter id"))?;
            let shelf = Shelf::load(s)?;
            let artist = shelf.artists().into_iter().find(|a| a.id == id).ok_or_else(|| fail(70, "no such artist"))?;
            let mut v = artist_json(&artist);
            v["album"] = json!(artist.albums.iter().map(album_json).collect::<Vec<_>>());
            json!({ "artist": v })
        }
        "getAlbum" => {
            let id = one(p, "id").ok_or_else(|| fail(10, "missing parameter id"))?;
            let shelf = Shelf::load(s)?;
            let album = shelf.albums().into_iter().find(|a| a.id == id).ok_or_else(|| fail(70, "no such album"))?;
            let mut v = album_json(&album);
            v["song"] = json!(album.songs.iter().map(|t| song_json(t)).collect::<Vec<_>>());
            json!({ "album": v })
        }
        "getSong" => {
            let id = num::<i64>(p, "id", -1);
            let track = s.app.db().track_by_id(id)?;
            let track = track.filter(playable).ok_or_else(|| fail(70, "no such song"))?;
            json!({ "song": song_json(&track) })
        }
        "getAlbumList" | "getAlbumList2" => {
            let key = if method == "getAlbumList2" { "albumList2" } else { "albumList" };
            let shelf = Shelf::load(s)?;
            let mut albums = shelf.albums();
            match one(p, "type").unwrap_or("alphabeticalByName") {
                "alphabeticalByArtist" => albums.sort_by_key(|a| (a.artist.to_lowercase(), a.name.to_lowercase())),
                "byYear" => albums.sort_by_key(|a| a.year),
                // No play counts or added dates per album; the newest rows stand in for all three.
                "newest" | "recent" | "frequent" => albums.sort_by_key(|a| std::cmp::Reverse(a.newest)),
                "random" => {
                    let rs = RandomState::new();
                    albums.sort_by_key(|a| rs.hash_one(&a.id));
                }
                _ => albums.sort_by_key(|a| a.name.to_lowercase()),
            }
            let (size, offset) = (num::<usize>(p, "size", 10).min(500), num::<usize>(p, "offset", 0));
            json!({ key: { "album": albums.iter().skip(offset).take(size).map(album_json).collect::<Vec<_>>() } })
        }
        "search2" | "search3" => {
            let key = if method == "search3" { "searchResult3" } else { "searchResult2" };
            // Some clients send `""` to mean "everything".
            let q = one(p, "query").unwrap_or("").trim().trim_matches('"').to_lowercase();
            let hit = |s: &str| q.is_empty() || s.to_lowercase().contains(&q);
            let shelf = Shelf::load(s)?;
            let page = |p: &Params, what: &str| {
                (num::<usize>(p, &format!("{what}Offset"), 0), num::<usize>(p, &format!("{what}Count"), 20))
            };
            let (ao, ac) = page(p, "artist");
            let (lo, lc) = page(p, "album");
            let (so, sc) = page(p, "song");
            json!({ key: {
                "artist": shelf.artists().iter().filter(|a| hit(a.name)).skip(ao).take(ac).map(artist_json).collect::<Vec<_>>(),
                "album": shelf.albums().iter().filter(|a| hit(a.name) || hit(a.artist)).skip(lo).take(lc).map(album_json).collect::<Vec<_>>(),
                "song": shelf.tracks.iter().filter(|t| hit(&t.title) || hit(&t.artist) || hit(&t.album)).skip(so).take(sc).map(song_json).collect::<Vec<_>>(),
            } })
        }
        "getPlaylists" => {
            let all = s.app.db().playlists()?;
            let mut out = vec![];
            for pl in &all {
                let tracks = s.app.db().playlist_tracks(pl.id)?;
                out.push(playlist_json(cfg, pl, &all, &tracks, who));
            }
            json!({ "playlists": { "playlist": out } })
        }
        "getPlaylist" => {
            let id = num::<i64>(p, "id", -1);
            let all = s.app.db().playlists()?;
            let pl = all.iter().find(|q| q.id == id).ok_or_else(|| fail(70, "no such playlist"))?;
            let tracks = s.app.db().playlist_tracks(pl.id)?;
            let mut v = playlist_json(cfg, pl, &all, &tracks, who);
            v["entry"] = json!(tracks.iter().filter(|t| playable(t)).map(song_json).collect::<Vec<_>>());
            json!({ "playlist": v })
        }
        "stream" | "download" => return Ok(Reply::Raw(stream(s, p, req.expect("request kept for streaming")).await)),
        "getCoverArt" => return Ok(Reply::Raw(cover(s, p)?)),
        "scrobble" => {
            scrobble(s, p).await?;
            json!({})
        }
        // Accepted so clients' favourite buttons do not error; tapedeck keeps no stars.
        "star" | "unstar" | "setRating" => json!({}),
        "getStarred" | "getStarred2" => {
            json!({ (if method == "getStarred2" { "starred2" } else { "starred" }): { "artist": [], "album": [], "song": [] } })
        }
        "getNowPlaying" => json!({ "nowPlaying": { "entry": [] } }),
        "getGenres" => json!({ "genres": { "genre": [] } }),
        "getRandomSongs" => {
            let rs = RandomState::new();
            let mut tracks = Shelf::load(s)?.tracks;
            tracks.sort_by_key(|t| rs.hash_one(t.id));
            let size = num::<usize>(p, "size", 10).min(500);
            json!({ "randomSongs": { "song": tracks.iter().take(size).map(song_json).collect::<Vec<_>>() } })
        }
        _ => return Err(fail(0, format!("tapedeck does not implement {method}"))),
    };
    Ok(Reply::Json(v))
}

/// `Ok(admin)`: whether this client may do everything. No `[server.subsonic] user` means every
/// client is anonymous (its credentials are ignored); the admin bearer token also passes.
fn authorize(cfg: &Subsonic, bearer_admin: bool, p: &Params) -> Result<bool, Fail> {
    let Some(user) = cfg.user.as_deref() else { return Ok(bearer_admin) };
    if bearer_admin {
        return Ok(true);
    }
    if one(p, "apiKey").is_some() {
        return Err(fail(42, "API keys are not supported; sign in with username and password"));
    }
    let u = one(p, "u").ok_or_else(|| fail(10, "missing parameter u"))?;
    let password = cfg.password.as_deref().unwrap_or("");
    let ok = match (one(p, "t"), one(p, "s"), one(p, "p")) {
        (Some(t), Some(salt), _) => t.eq_ignore_ascii_case(&token(password, salt)),
        (None, None, Some(pw)) => plain(pw) == password,
        _ => return Err(fail(10, "missing parameters t and s (or p)")),
    };
    if u != user || !ok {
        return Err(fail(40, "Wrong username or password"));
    }
    Ok(true)
}

/// Subsonic's token: `md5(password + salt)` in lowercase hex.
pub fn token(password: &str, salt: &str) -> String {
    let mut h = Md5::new();
    h.update(password.as_bytes());
    h.update(salt.as_bytes());
    format!("{:x}", h.finalize())
}

/// The `p` parameter, plain or `enc:<hex>`.
fn plain(p: &str) -> String {
    let decode = |h: &str| {
        (0..h.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(h.get(i..i + 2)?, 16).ok())
            .collect::<Option<Vec<u8>>>()
            .and_then(|b| String::from_utf8(b).ok())
    };
    p.strip_prefix("enc:").and_then(decode).unwrap_or_else(|| p.to_string())
}

fn playable(t: &Track) -> bool {
    t.path.is_some() && matches!(t.status.as_str(), "ready" | "external")
}

/// The playable library, grouped on demand. Albums and artists are keyed by their MusicBrainz
/// ids, which the files carry anyway, so ids stay stable across restarts and renames.
// ponytail: regrouped on every request; cache behind a tracks-table version if a library of
// tens of thousands makes browsing slow.
struct Shelf {
    tracks: Vec<Track>,
}

struct Album<'a> {
    id: String,
    name: &'a str,
    artist: &'a str,
    artist_id: String,
    year: Option<i64>,
    /// Highest track id, a stand-in for "added most recently".
    newest: i64,
    songs: Vec<&'a Track>,
}

struct ArtistRow<'a> {
    id: String,
    name: &'a str,
    albums: Vec<Album<'a>>,
}

fn album_id(t: &Track) -> String {
    if t.album_mbid.is_empty() { format!("{}\u{1f}{}", t.album_artist, t.album) } else { t.album_mbid.clone() }
}

fn artist_id(t: &Track) -> String {
    if t.album_artist_mbid.is_empty() { t.album_artist.clone() } else { t.album_artist_mbid.clone() }
}

impl Shelf {
    fn load(s: &Shared) -> Result<Shelf, Fail> {
        let mut tracks = s.app.db().all_tracks()?;
        tracks.retain(playable);
        Ok(Shelf { tracks })
    }

    fn albums(&self) -> Vec<Album<'_>> {
        let mut by: BTreeMap<String, Album<'_>> = BTreeMap::new();
        for t in &self.tracks {
            let a = by.entry(album_id(t)).or_insert_with(|| Album {
                id: album_id(t),
                name: &t.album,
                artist: &t.album_artist,
                artist_id: artist_id(t),
                year: t.year,
                newest: t.id,
                songs: vec![],
            });
            a.newest = a.newest.max(t.id);
            a.year = a.year.or(t.year);
            a.songs.push(t);
        }
        let mut albums: Vec<_> = by.into_values().collect();
        for a in &mut albums {
            a.songs.sort_by_key(|t| (t.track_no.unwrap_or(i64::MAX), t.title.to_lowercase()));
        }
        albums.sort_by_key(|a| a.name.to_lowercase());
        albums
    }

    fn artists(&self) -> Vec<ArtistRow<'_>> {
        let mut by: BTreeMap<String, ArtistRow<'_>> = BTreeMap::new();
        for a in self.albums() {
            let row = by.entry(a.artist_id.clone()).or_insert_with(|| ArtistRow {
                id: a.artist_id.clone(),
                name: a.artist,
                albums: vec![],
            });
            row.albums.push(a);
        }
        let mut artists: Vec<_> = by.into_values().collect();
        artists.sort_by_key(|a| a.name.to_lowercase());
        artists
    }
}

/// `getArtists` groups by first letter; digits and symbols land under `#`.
fn index(artists: &[ArtistRow<'_>]) -> Value {
    let mut by: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for a in artists {
        let first = a.name.chars().next().filter(|c| c.is_alphabetic()).map(|c| c.to_uppercase().to_string());
        by.entry(first.unwrap_or_else(|| "#".into())).or_default().push(artist_json(a));
    }
    json!(by.into_iter().map(|(name, artist)| json!({ "name": name, "artist": artist })).collect::<Vec<_>>())
}

fn cover_id(album_id: &str) -> Option<&str> {
    crate::library::is_uuid(album_id).then_some(album_id)
}

fn artist_json(a: &ArtistRow<'_>) -> Value {
    json!({
        "id": a.id,
        "name": a.name,
        "albumCount": a.albums.len(),
        "coverArt": a.albums.first().and_then(|al| cover_id(&al.id)),
        "musicBrainzId": crate::library::is_uuid(&a.id).then_some(&a.id),
    })
}

fn album_json(a: &Album<'_>) -> Value {
    json!({
        "id": a.id,
        "parent": a.artist_id,
        "isDir": true,
        "name": a.name,
        "album": a.name,
        "title": a.name,
        "artist": a.artist,
        "artistId": a.artist_id,
        "coverArt": cover_id(&a.id),
        "songCount": a.songs.len(),
        "duration": a.songs.iter().map(|t| t.duration_ms.unwrap_or(0) / 1000).sum::<i64>(),
        "year": a.year,
        "musicBrainzId": cover_id(&a.id),
    })
}

fn song_json(t: &Track) -> Value {
    let path = t.path.as_deref().unwrap_or("");
    let suffix = path.rsplit_once('.').map(|(_, e)| e.to_lowercase()).unwrap_or_default();
    let content_type = match suffix.as_str() {
        "opus" | "ogg" | "oga" => "audio/ogg",
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "m4a" | "aac" | "mp4" => "audio/mp4",
        "wav" => "audio/wav",
        _ => "application/octet-stream",
    };
    let album_id = album_id(t);
    json!({
        "id": t.id.to_string(),
        "parent": album_id,
        "isDir": false,
        "isVideo": false,
        "type": "music",
        "title": t.title,
        "album": t.album,
        "artist": t.artist,
        "albumArtist": t.album_artist,
        "track": t.track_no,
        "year": t.year,
        "coverArt": cover_id(&album_id),
        "duration": t.duration_ms.map(|ms| ms / 1000),
        "contentType": content_type,
        "suffix": suffix,
        "path": path,
        "albumId": album_id,
        "artistId": artist_id(t),
        "musicBrainzId": t.recording_mbid,
    })
}

fn playlist_json(
    cfg: &crate::config::Config,
    pl: &crate::db::Playlist,
    all: &[crate::db::Playlist],
    tracks: &[Track],
    owner: &str,
) -> Value {
    let playable: Vec<_> = tracks.iter().filter(|t| playable(t)).collect();
    json!({
        "id": pl.id.to_string(),
        "name": remote_title(cfg, pl, all),
        "comment": pl.name,
        "owner": owner,
        "public": true,
        "songCount": playable.len(),
        "duration": playable.iter().map(|t| t.duration_ms.unwrap_or(0) / 1000).sum::<i64>(),
        "created": pl.fetched_at,
        "changed": pl.fetched_at,
        "coverArt": playable.first().and_then(|t| cover_id(&t.album_mbid)),
    })
}

/// The file, own or external, with Range support, like `/stream/{id}`.
async fn stream(s: &Shared, p: &Params, req: Request) -> Response {
    let id = num::<i64>(p, "id", -1);
    let track = s.app.db().track_by_id(id).ok().flatten();
    let Some(rel) = track.filter(playable).and_then(|t| t.path) else {
        return envelope(json!({ "status": "failed", "error": { "code": 70, "message": "no such song" } }), json!({}));
    };
    let file = s.app.config.library.join(rel);
    match ServeFile::new(&file).oneshot(req).await {
        Ok(res) => res.into_response(),
        Err(e) => envelope(json!({ "status": "failed", "error": { "code": 0, "message": e.to_string() } }), json!({})),
    }
}

/// Covers come from the Cover Art Archive by release id: a redirect, nothing stored.
fn cover(s: &Shared, p: &Params) -> Result<Response, Fail> {
    let id = one(p, "id").ok_or_else(|| fail(10, "missing parameter id"))?;
    let mbid = match id.parse::<i64>() {
        Ok(n) => s.app.db().track_by_id(n)?.map(|t| t.album_mbid).unwrap_or_default(),
        Err(_) => id.to_string(),
    };
    if !crate::library::is_uuid(&mbid) {
        return Err(fail(70, "no cover art for this item"));
    }
    let size = match num::<u32>(p, "size", 500) {
        s if s <= 250 => 250,
        s if s <= 500 => 500,
        _ => 1200,
    };
    Ok(Redirect::temporary(&format!("https://coverartarchive.org/release/{mbid}/front-{size}")).into_response())
}

/// `submission=false` is "playing now", anything else a listen; both go through `td listen`,
/// so the daemon's ListenBrainz token and queue apply.
// ponytail: the client's `time` is ignored, the listen is stamped now; wire it through when a
// client that submits backlogs shows up.
async fn scrobble(s: &Shared, p: &Params) -> Result<(), Fail> {
    let now = one(p, "submission").is_some_and(|v| v == "false");
    let ids: Vec<i64> = p.iter().filter(|(k, _)| k == "id").filter_map(|(_, v)| v.parse().ok()).collect();
    if ids.is_empty() {
        return Err(fail(10, "missing parameter id"));
    }
    tracing::info!(
        "subsonic: {} {:?} from {}",
        if now { "playing now" } else { "scrobble" },
        ids,
        one(p, "c").unwrap_or("?")
    );
    for track in ids {
        let (tx, mut rx) = mpsc::channel(16);
        let app = s.app.clone();
        let run = tokio::spawn(async move { app.run(Command::Listen { track, now }, tx).await });
        while let Some(ev) = rx.recv().await {
            if let Event::Error { msg } = ev {
                return Err(fail(0, msg));
            }
        }
        let _ = run.await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(pairs: &[(&str, &str)]) -> Params {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn token_matches_the_spec_example() {
        // From the Subsonic API docs: password "sesame", salt "c19b2d" → 26719a1196d2a940705a59634eb18eab.
        assert_eq!(token("sesame", "c19b2d"), "26719a1196d2a940705a59634eb18eab");
        assert_eq!(plain("enc:736573616d65"), "sesame");
        assert_eq!(plain("sesame"), "sesame");
    }

    #[test]
    fn anonymous_without_a_user_and_strict_with_one() {
        let open = Subsonic::default();
        assert!(!authorize(&open, false, &params(&[("u", "x"), ("p", "nope")])).unwrap());
        assert!(authorize(&open, true, &params(&[])).unwrap());

        let locked = Subsonic { user: Some("me".into()), password: Some("sesame".into()), password_file: None };
        assert!(
            authorize(
                &locked,
                false,
                &params(&[("u", "me"), ("t", "26719a1196d2a940705a59634eb18eab"), ("s", "c19b2d")])
            )
            .unwrap()
        );
        assert!(authorize(&locked, false, &params(&[("u", "me"), ("p", "enc:736573616d65")])).unwrap());
        assert!(authorize(&locked, true, &params(&[])).unwrap(), "the admin bearer token passes");
        let code = |p: &[(&str, &str)]| authorize(&locked, false, &params(p)).err().map(|f| f.code);
        assert_eq!(code(&[("u", "me"), ("p", "wrong")]), Some(40));
        assert_eq!(code(&[("u", "you"), ("p", "sesame")]), Some(40));
        assert_eq!(code(&[("u", "me")]), Some(10));
        assert_eq!(code(&[("apiKey", "k")]), Some(42));
    }

    #[test]
    fn shelf_groups_by_release_and_artist_ids() {
        let t = |id, title: &str, album: &str, album_mbid: &str, artist: &str, artist_mbid: &str, no| Track {
            id,
            title: title.into(),
            album: album.into(),
            album_mbid: album_mbid.into(),
            artist: artist.into(),
            album_artist: artist.into(),
            album_artist_mbid: artist_mbid.into(),
            track_no: Some(no),
            path: Some(format!("{title}.opus")),
            status: "ready".into(),
            duration_ms: Some(60_000),
            ..Default::default()
        };
        let a = "11111111-1111-1111-1111-111111111111";
        let b = "22222222-2222-2222-2222-222222222222";
        let ar = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let shelf = Shelf {
            tracks: vec![
                t(1, "Two", "Album A", a, "Zed", ar, 2),
                t(2, "One", "Album A", a, "Zed", ar, 1),
                t(3, "Solo", "Album B", b, "Zed", ar, 1),
                t(4, "Loose", "Video", "", "99 Problems", "", 1),
            ],
        };
        let albums = shelf.albums();
        assert_eq!(albums.iter().map(|a| a.name).collect::<Vec<_>>(), ["Album A", "Album B", "Video"]);
        assert_eq!(albums[0].songs.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(), ["One", "Two"]);
        assert_eq!(albums[0].newest, 2);
        let artists = shelf.artists();
        assert_eq!(artists.iter().map(|a| a.name).collect::<Vec<_>>(), ["99 Problems", "Zed"]);
        assert_eq!(artists[1].id, ar);
        assert_eq!(artists[1].albums.len(), 2);
        let idx = index(&artists);
        assert_eq!(idx[0]["name"], "#");
        assert_eq!(idx[1]["name"], "Z");
        let song = song_json(&shelf.tracks[0]);
        assert_eq!(song["contentType"], "audio/ogg");
        assert_eq!(song["coverArt"], a);
        let mut loose = song_json(&shelf.tracks[3]);
        strip_nulls(&mut loose);
        assert!(loose.get("coverArt").is_none(), "no release id, no cover key: {loose}");
    }
}
