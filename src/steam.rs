//! Steam soundtracks as a second source next to MusicBrainz. Steam sells game soundtracks as
//! "Music" apps: plain MP3 or FLAC, tagged, often with embedded art, no DRM, installed by the
//! Steam client under `steamapps/music` or fetched headlessly by steamcmd. MusicBrainz knows
//! few of them, so they come in on their own terms: the files' tags are trusted and kept,
//! nothing is retagged, ids are synthetic (`steam:<key>:<file>`), covers are the embedded
//! pictures or a cover.jpg next to the files. Downstream (library layout, m3u, Plex, Subsonic,
//! TUI) they are ordinary ready tracks.
//!
//! `[steam] path` imports what the Steam client already installed, one subfolder per
//! soundtrack. `[steam] username` lets steamcmd list every app the account owns, keep the ones
//! the store calls music, download each new one into a staging folder and import it; steamcmd
//! has to be logged in once by hand (Steam Guard), after that its cached session works
//! unattended. `td steam list` shows what is known, `td steam sync` does the work, the daemon
//! repeats it on `[steam] interval`.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use lofty::{prelude::*, probe::Probe};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc::Sender;

use crate::{
    App, Event,
    app::info,
    db::{NewTrack, Track},
    external::chrono_now,
    library,
    m3u::slug,
};

const AUDIO_EXT: &[&str] = &["mp3", "flac", "ogg", "opus", "m4a", "wav", "aac"];
const COVER_NAMES: &[&str] = &["cover.jpg", "cover.png", "folder.jpg", "front.jpg", "Cover.jpg", "Folder.jpg"];

/// One soundtrack after an import pass.
#[derive(Serialize, Debug, Clone)]
pub struct SoundtrackReport {
    pub key: String,
    pub album: String,
    pub album_artist: String,
    pub imported: usize,
    pub already: usize,
}

/// A file's own tags, with the folder as the fallback for whatever is missing.
struct Scanned {
    path: PathBuf,
    title: String,
    artist: String,
    album: String,
    album_artist: String,
    track_no: Option<i64>,
    year: Option<i64>,
    duration_ms: Option<i64>,
}

fn read_tags(path: &Path, folder: &str) -> Scanned {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let (stem_no, stem_title) = split_stem(stem);
    let mut s = Scanned {
        path: path.to_path_buf(),
        title: stem_title,
        artist: String::new(),
        album: folder.to_string(),
        album_artist: String::new(),
        track_no: stem_no,
        year: None,
        duration_ms: None,
    };
    if let Ok(file) = Probe::open(path).and_then(|p| p.read()) {
        s.duration_ms = Some(file.properties().duration().as_millis() as i64);
        if let Some(tag) = file.primary_tag().or_else(|| file.first_tag()) {
            let text = |v: Option<std::borrow::Cow<'_, str>>| v.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
            if let Some(t) = text(tag.title()) {
                s.title = t;
            }
            if let Some(a) = text(tag.artist()) {
                s.artist = a;
            }
            if let Some(a) = text(tag.album()) {
                s.album = a;
            }
            if let Some(a) = tag.get_string(ItemKey::AlbumArtist).map(str::trim).filter(|a| !a.is_empty()) {
                s.album_artist = a.to_string();
            }
            if let Some(n) = tag.track() {
                s.track_no = Some(n as i64);
            }
            let date = tag.get_string(ItemKey::RecordingDate).or_else(|| tag.get_string(ItemKey::OriginalReleaseDate));
            if let Some(y) = date.and_then(|d| d.get(..4)).and_then(|y| y.parse::<i64>().ok()) {
                s.year = Some(y);
            }
        }
    }
    if s.album_artist.is_empty() {
        s.album_artist = if s.artist.is_empty() { folder.to_string() } else { s.artist.clone() };
    }
    if s.artist.is_empty() {
        s.artist = s.album_artist.clone();
    }
    s
}

/// "07 - Resurrections" / "07. Resurrections" / "07 Resurrections" → (7, "Resurrections").
fn split_stem(stem: &str) -> (Option<i64>, String) {
    let digits: String = stem.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return (None, stem.to_string());
    }
    let rest = stem[digits.len()..].trim_start_matches([' ', '-', '.', '_']).trim();
    if rest.is_empty() {
        return (digits.parse().ok(), stem.to_string());
    }
    (digits.parse().ok(), rest.to_string())
}

/// The value most files agree on, so one odd tag does not split an album.
fn majority<'a>(values: impl Iterator<Item = &'a str>) -> String {
    let mut counts: Vec<(&str, usize)> = vec![];
    for v in values.filter(|v| !v.is_empty()) {
        match counts.iter_mut().find(|(k, _)| *k == v) {
            Some((_, n)) => *n += 1,
            None => counts.push((v, 1)),
        }
    }
    counts.into_iter().max_by_key(|(_, n)| *n).map(|(v, _)| v.to_string()).unwrap_or_default()
}

fn audio_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = library::walk(dir)
        .into_iter()
        .map(|(p, _, _)| p)
        .filter(|p| {
            p.extension().and_then(|e| e.to_str()).is_some_and(|e| AUDIO_EXT.contains(&e.to_lowercase().as_str()))
        })
        .collect();
    files.sort();
    files
}

/// Copy a file to its templated place, keeping its tags. Like `library::file_track` minus the
/// retag: these files are right as they are, and MusicBrainz fields would only hold made-up ids.
fn copy_in(library: &Path, template: &str, t: &Track, src: &Path) -> Result<PathBuf> {
    let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("bin");
    let rel = library::unique(library, &library::render_template(template, t, ext)?);
    let dest = library.join(&rel);
    std::fs::create_dir_all(dest.parent().context("template has no directory part")?)?;
    std::fs::copy(src, &dest).with_context(|| format!("copying {} → {}", src.display(), dest.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o644))?;
    Ok(rel)
}

/// The album's picture for a Subsonic client: the first embedded one in `file`, else a
/// cover.jpg / folder.jpg / cover.png next to it.
pub(crate) fn cover_for(file: &Path) -> Option<Vec<u8>> {
    if let Ok(f) = Probe::open(file).and_then(|p| p.read()) {
        for tag in f.tags() {
            if let Some(pic) = tag.pictures().first() {
                return Some(pic.data().to_vec());
            }
        }
    }
    let dir = file.parent()?;
    COVER_NAMES.iter().find_map(|n| std::fs::read(dir.join(n)).ok())
}

/// `steam:<key>` album ids and `steam:<key>:<file>` track ids.
pub fn is_steam_id(id: &str) -> bool {
    id.starts_with("steam:")
}

impl App {
    /// Copy the audio files under `dir` into the library as the soundtrack `key` (a Steam app
    /// id, or the folder's slug), trusting their tags. Files imported before are left alone.
    pub(crate) async fn steam_import(&self, key: &str, dir: &Path, tx: &Sender<Event>) -> Result<SoundtrackReport> {
        let files = audio_files(dir);
        ensure!(!files.is_empty(), "{}: no audio files", dir.display());
        let folder = dir.file_name().and_then(|s| s.to_str()).unwrap_or(key).to_string();
        let scanned: Vec<Scanned> = files.iter().map(|p| read_tags(p, &folder)).collect();
        let album = majority(scanned.iter().map(|s| s.album.as_str()));
        let album_artist = majority(scanned.iter().map(|s| s.album_artist.as_str()));
        let year = scanned.iter().find_map(|s| s.year);
        let album_mbid = format!("steam:{key}");
        let album_artist_mbid = format!("steam:artist:{}", slug(&album_artist));
        let (mut imported, mut already) = (0, 0);
        for s in &scanned {
            let within = s.path.strip_prefix(dir).unwrap_or(&s.path).to_string_lossy().into_owned();
            let recording = format!("steam:{key}:{}", slug(&within));
            let existing = self.db().track_by_mbid(&recording)?;
            if let Some(t) = existing
                && t.path.as_deref().is_some_and(|p| self.config.library.join(p).exists())
            {
                already += 1;
                continue;
            }
            let id = self.db().insert_track(&NewTrack {
                recording_mbid: &recording,
                title: &s.title,
                track_no: s.track_no,
                artist: &s.artist,
                album_artist_mbid: &album_artist_mbid,
                album_artist: &album_artist,
                album_mbid: &album_mbid,
                album: &album,
                year,
                artist_mbid: None,
                duration_ms: s.duration_ms,
            })?;
            let track = Track {
                id,
                recording_mbid: recording,
                title: s.title.clone(),
                artist: s.artist.clone(),
                album_artist: album_artist.clone(),
                album: album.clone(),
                year,
                track_no: s.track_no,
                album_mbid: album_mbid.clone(),
                album_artist_mbid: album_artist_mbid.clone(),
                duration_ms: s.duration_ms,
                ..Default::default()
            };
            let rel = copy_in(&self.config.library, &self.config.path_template, &track, &s.path)?;
            self.db().mark_ready(id, &rel.to_string_lossy(), None)?;
            imported += 1;
            // The folder's artwork travels with the first file: Plex and Subsonic read it from there.
            if let (Some(src_dir), Some(dest_dir)) = (s.path.parent(), self.config.library.join(&rel).parent())
                && let Some(art) = COVER_NAMES.iter().map(|n| src_dir.join(n)).find(|p| p.is_file())
                && !COVER_NAMES.iter().any(|n| dest_dir.join(n).is_file())
            {
                let ext = art.extension().and_then(|e| e.to_str()).unwrap_or("jpg").to_lowercase();
                let _ = std::fs::copy(&art, dest_dir.join(format!("cover.{ext}")));
            }
        }
        info(tx, format!("steam: {album_artist} – {album}: {imported} imported, {already} already there")).await?;
        Ok(SoundtrackReport { key: key.to_string(), album, album_artist, imported, already })
    }

    fn steam_staging(&self) -> PathBuf {
        self.config.steam.staging.clone().unwrap_or_else(|| {
            self.config.database_path().parent().map(Path::to_path_buf).unwrap_or_default().join("steam")
        })
    }

    /// Import the Steam client's folder, then whatever the account owns and has not been
    /// imported yet.
    pub(crate) async fn steam_sync(&self, tx: &Sender<Event>) -> Result<Vec<SoundtrackReport>> {
        let cfg = &self.config.steam;
        ensure!(cfg.path.is_some() || cfg.username.is_some(), "no [steam] path or username configured");
        let mut reports = vec![];
        if let Some(root) = &cfg.path {
            for dir in soundtrack_dirs(root) {
                let key = slug(dir.file_name().and_then(|s| s.to_str()).unwrap_or("soundtrack"));
                match self.steam_import(&key, &dir, tx).await {
                    Ok(r) => reports.push(r),
                    Err(e) => info(tx, format!("steam: {}: {e:#}", dir.display())).await?,
                }
            }
        }
        if let Some(user) = &cfg.username {
            for (app, name) in self.steam_owned_music(user, tx).await? {
                if self.db().meta_get(&format!("steam.done.{app}"))?.is_some() {
                    continue;
                }
                info(tx, format!("steam: downloading {name} ({app})")).await?;
                let dir = self.steam_staging().join(app.to_string());
                if let Err(e) = steamcmd_download(&cfg.steamcmd_bin, user, app, &dir).await {
                    info(tx, format!("steam: {name}: {e:#}")).await?;
                    continue;
                }
                match self.steam_import(&app.to_string(), &dir, tx).await {
                    Ok(r) => {
                        self.db().meta_set(&format!("steam.done.{app}"), &chrono_now())?;
                        let _ = std::fs::remove_dir_all(&dir);
                        reports.push(r);
                    }
                    Err(e) => info(tx, format!("steam: {name}: {e:#}")).await?,
                }
            }
        }
        Ok(reports)
    }

    /// What `td steam list` shows: the folders under `path` and the owned soundtracks with
    /// their import state.
    pub(crate) async fn steam_list(&self, tx: &Sender<Event>) -> Result<Value> {
        let cfg = &self.config.steam;
        ensure!(cfg.path.is_some() || cfg.username.is_some(), "no [steam] path or username configured");
        let folders: Vec<Value> = cfg
            .path
            .iter()
            .flat_map(|root| soundtrack_dirs(root))
            .map(|dir| {
                let name = dir.file_name().and_then(|s| s.to_str()).unwrap_or("").to_string();
                let key = format!("steam:{}", slug(&name));
                let imported =
                    self.db().all_tracks().map(|ts| ts.iter().filter(|t| t.album_mbid == key).count()).unwrap_or(0);
                serde_json::json!({ "folder": name, "files": audio_files(&dir).len(), "imported": imported })
            })
            .collect();
        let mut owned = vec![];
        if let Some(user) = &cfg.username {
            for (app, name) in self.steam_owned_music(user, tx).await? {
                let done = self.db().meta_get(&format!("steam.done.{app}"))?;
                owned.push(serde_json::json!({ "appid": app, "name": name, "imported": done }));
            }
        }
        Ok(serde_json::json!({ "folders": folders, "owned": owned }))
    }

    /// Every owned app that the store calls music, by asking steamcmd for the account's
    /// licenses and the store for each app's type (remembered, so the store is asked once per app).
    async fn steam_owned_music(&self, user: &str, tx: &Sender<Event>) -> Result<Vec<(u64, String)>> {
        let out = steamcmd(&self.config.steam.steamcmd_bin, &["+login", user, "+licenses_print", "+quit"]).await?;
        let apps = parse_licenses(&out);
        ensure!(
            !apps.is_empty(),
            "steamcmd listed no licenses for {user}; is it logged in? (run `steamcmd +login {user}` once by hand)"
        );
        let mut music = vec![];
        let mut asked = 0;
        for app in apps {
            let key = format!("steam.app.{app}");
            // Bound first: a database guard must not live across the awaits below.
            let cached = self.db().meta_get(&key)?;
            let entry = match cached {
                Some(v) => v,
                None => {
                    if asked > 0 {
                        // The store allows a couple of hundred lookups per five minutes.
                        tokio::time::sleep(Duration::from_millis(1500)).await;
                    }
                    asked += 1;
                    match store_type(app).await {
                        Ok(v) => {
                            self.db().meta_set(&key, &v)?;
                            v
                        }
                        Err(e) => {
                            info(
                                tx,
                                format!(
                                    "steam: store lookup for {app} failed ({e:#}); the rest waits for the next run"
                                ),
                            )
                            .await?;
                            break;
                        }
                    }
                }
            };
            if let Some(name) = entry.strip_prefix("music\t") {
                music.push((app, name.to_string()));
            }
        }
        if asked > 0 {
            info(tx, format!("steam: asked the store about {asked} apps; {} soundtracks owned", music.len())).await?;
        }
        Ok(music)
    }
}

/// Subfolders of the Steam client's `steamapps/music`, one per soundtrack.
fn soundtrack_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect())
        .unwrap_or_default();
    dirs.sort();
    dirs
}

/// App ids from `licenses_print` output (" - Apps    : 1234, 5678, (2 in total)").
fn parse_licenses(out: &str) -> BTreeSet<u64> {
    out.lines()
        .filter(|l| l.trim_start().starts_with("- Apps"))
        .filter_map(|l| l.split_once(':').map(|(_, ids)| ids))
        .flat_map(|ids| ids.split(',').map(str::trim).filter_map(|w| w.parse::<u64>().ok()).collect::<Vec<_>>())
        .collect()
}

/// "music\t<name>" for a soundtrack app, "other" for anything else.
async fn store_type(app: u64) -> Result<String> {
    let url = format!("https://store.steampowered.com/api/appdetails?appids={app}&filters=basic");
    let res = crate::http().get(&url).send().await?;
    ensure!(res.status() != reqwest::StatusCode::TOO_MANY_REQUESTS, "store rate limit");
    let v: Value = res.error_for_status()?.json().await?;
    let data = &v[app.to_string()]["data"];
    Ok(match (data["type"].as_str(), data["name"].as_str()) {
        (Some("music"), Some(name)) => format!("music\t{name}"),
        _ => "other".to_string(),
    })
}

async fn steamcmd(bin: &str, args: &[&str]) -> Result<String> {
    let out = tokio::process::Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("running {bin} (set steam.steamcmd_bin or install steamcmd)"))?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if text.contains("Steam Guard") || text.contains("Cached credentials not found") || text.contains("Login Failure") {
        bail!(
            "steamcmd is not logged in; run `{bin} +login <user> +quit` once by hand as the user tapedeck runs as, then try again"
        );
    }
    if !out.status.success() {
        let tail: Vec<&str> = text.lines().rev().take(5).collect::<Vec<_>>().into_iter().rev().collect();
        bail!("steamcmd failed ({}): {}", out.status, tail.join(" | "));
    }
    Ok(text)
}

/// Fetch one app into `dir`. Soundtracks only ship Windows depots, so the platform is forced.
async fn steamcmd_download(bin: &str, user: &str, app: u64, dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let dir_s = dir.to_string_lossy().into_owned();
    let app_s = app.to_string();
    let out = steamcmd(
        bin,
        &[
            "+@sSteamCmdForcePlatformType",
            "windows",
            "+force_install_dir",
            &dir_s,
            "+login",
            user,
            "+app_update",
            &app_s,
            "validate",
            "+quit",
        ],
    )
    .await?;
    ensure!(
        out.contains("fully installed") || !audio_files(dir).is_empty(),
        "steamcmd did not report the app as installed: {}",
        out.lines().rev().take(3).collect::<Vec<_>>().join(" | ")
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stems_and_majorities() {
        assert_eq!(split_stem("07 - Resurrections"), (Some(7), "Resurrections".into()));
        assert_eq!(split_stem("07. Resurrections"), (Some(7), "Resurrections".into()));
        assert_eq!(split_stem("7 Resurrections"), (Some(7), "Resurrections".into()));
        assert_eq!(split_stem("Resurrections"), (None, "Resurrections".into()));
        assert_eq!(split_stem("1999"), (Some(1999), "1999".into()));
        assert_eq!(majority(["A", "B", "A", ""].into_iter()), "A");
        assert_eq!(majority(["", ""].into_iter()), "");
    }

    #[test]
    fn licenses_print_is_parsed() {
        let out = "License packageID 12345:\n - State   : Active( flags 0 )\n - Apps    : 1234, 5678, (2 in total)\n - Depots  : 1235, (1 in total)\nLicense packageID 99:\n - Apps    : 42, (1 in total)\n";
        assert_eq!(parse_licenses(out).into_iter().collect::<Vec<_>>(), [42, 1234, 5678]);
        assert!(parse_licenses("nothing here").is_empty());
    }

    #[test]
    fn tags_fall_back_to_folder_and_stem() {
        let dir = std::env::temp_dir().join(format!("td-steam-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("03 - Awake.mp3");
        std::fs::write(&f, b"not audio").unwrap();
        let s = read_tags(&f, "Celeste Original Soundtrack");
        assert_eq!((s.title.as_str(), s.track_no), ("Awake", Some(3)));
        assert_eq!(s.album, "Celeste Original Soundtrack");
        assert_eq!(s.album_artist, "Celeste Original Soundtrack");
        assert_eq!(s.artist, s.album_artist);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
