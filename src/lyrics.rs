//! Lyrics for the library, so Plexamp, Subsonic apps and the TUI can show them. Plex reads
//! sidecar files only: `<same stem>.lrc` (synced) or `.txt` (plain) next to the audio, picked
//! up on an album metadata refresh. The source is LRCLIB (lrclib.net): open data, no token,
//! synced and plain lyrics and an "instrumental" flag; looked up by artist, title, album and
//! length, then by search as a fallback. Everything found is kept in the `lyrics` table, so
//! Subsonic clients and the TUI get lyrics even for tracks nothing may be written next to
//! (Lidarr's, by default), and a miss is retried only after `[lyrics] retry_after`.
//!
//! Rules: an existing sidecar wins and is indexed as source "file"; sidecars are written only
//! into tapedeck's own library unless `[lyrics] external = "missing-sidecars"`; an existing
//! sidecar is never overwritten without `--force`; audio files are never touched; a sidecar
//! tapedeck wrote follows its track (library::move_track / remove_track).

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc::Sender;

use crate::{
    App, Event,
    app::{info, progress},
    db::{LyricsRow, Track},
    external::is_external,
};

const LRCLIB: &str = "https://lrclib.net/api";

/// What a provider answered.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Found {
    pub synced: Option<String>,
    pub plain: Option<String>,
    pub instrumental: bool,
    pub source: String,
    pub source_id: Option<String>,
}

/// The track as a provider needs it.
pub struct Query<'a> {
    pub title: &'a str,
    pub artist: &'a str,
    pub album: &'a str,
    /// Seconds.
    pub duration: Option<i64>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct LrclibHit {
    id: Option<i64>,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    instrumental: bool,
    plain_lyrics: Option<String>,
    synced_lyrics: Option<String>,
}

impl LrclibHit {
    fn into_found(self) -> Found {
        let clean = |s: Option<String>| s.filter(|s| !s.trim().is_empty());
        Found {
            synced: clean(self.synced_lyrics),
            plain: clean(self.plain_lyrics),
            instrumental: self.instrumental,
            source: "lrclib".into(),
            source_id: self.id.map(|i| i.to_string()),
        }
    }
}

/// Stop a pass: the provider asked us to back off.
#[derive(Debug)]
pub struct RateLimited(pub Duration);

impl std::fmt::Display for RateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "lrclib asked to retry after {:?}", self.0)
    }
}
impl std::error::Error for RateLimited {}

/// `GET /api/get` (exact match within about two seconds), then `/api/search` filtered by
/// length. `Ok(None)` is "not there", which is common and remembered.
pub async fn lrclib(q: &Query<'_>) -> Result<Option<Found>> {
    let http = crate::http();
    let mut url = reqwest::Url::parse(&format!("{LRCLIB}/get"))?;
    url.query_pairs_mut()
        .append_pair("artist_name", q.artist)
        .append_pair("track_name", q.title)
        .append_pair("album_name", q.album);
    if let Some(d) = q.duration {
        url.query_pairs_mut().append_pair("duration", &d.to_string());
    }
    let res = http.get(url).send().await.context("lrclib get")?;
    match res.status() {
        s if s.is_success() => {
            let hit: LrclibHit = res.json().await.context("lrclib get: bad json")?;
            return Ok(Some(hit.into_found()));
        }
        reqwest::StatusCode::TOO_MANY_REQUESTS => bail!(RateLimited(retry_after(&res))),
        // 404 is "not in the database"; `get` also asks external sources for unknown tracks and
        // answers 5xx when that fails, so anything else falls through to the search too.
        _ => {}
    }
    // Fallback: search by title and artist, keep a hit of the right length.
    let mut url = reqwest::Url::parse(&format!("{LRCLIB}/search"))?;
    url.query_pairs_mut().append_pair("track_name", q.title).append_pair("artist_name", q.artist);
    let res = http.get(url).send().await.context("lrclib search")?;
    if res.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        bail!(RateLimited(retry_after(&res)));
    }
    let hits: Vec<LrclibHit> = res.error_for_status().context("lrclib search")?.json().await.unwrap_or_default();
    let close = |h: &LrclibHit| match (q.duration, h.duration) {
        (Some(want), Some(got)) => (got - want as f64).abs() <= 2.0,
        _ => true,
    };
    Ok(hits
        .into_iter()
        .filter(close)
        .max_by_key(|h| (h.synced_lyrics.is_some(), h.plain_lyrics.is_some()))
        .map(LrclibHit::into_found))
}

fn retry_after(res: &reqwest::Response) -> Duration {
    res.headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(60))
}

/// `<stem>.lrc` and `<stem>.txt` next to an audio file.
pub fn sidecars(audio: &Path) -> [PathBuf; 2] {
    [audio.with_extension("lrc"), audio.with_extension("txt")]
}

/// The sidecar already on disk, if any: (extension, text).
pub fn read_sidecar(audio: &Path) -> Option<(&'static str, String)> {
    let [lrc, txt] = sidecars(audio);
    if let Ok(s) = std::fs::read_to_string(&lrc)
        && !s.trim().is_empty()
    {
        return Some(("lrc", s));
    }
    if let Ok(s) = std::fs::read_to_string(&txt)
        && !s.trim().is_empty()
    {
        return Some(("txt", s));
    }
    None
}

/// Write the sidecar for `found` next to `audio` (tmp + rename, umask applies). Returns the
/// extension written, `None` when there was nothing to write.
fn write_sidecar(audio: &Path, found: &Found, force: bool) -> Result<Option<&'static str>> {
    let (ext, body) = match (&found.synced, &found.plain) {
        (Some(s), _) => ("lrc", s),
        (None, Some(p)) => ("txt", p),
        (None, None) => return Ok(None),
    };
    let [lrc, txt] = sidecars(audio);
    if !force && (lrc.exists() || txt.exists()) {
        return Ok(None);
    }
    let path = if ext == "lrc" { lrc } else { txt };
    let tmp = path.with_extension(format!("{ext}.tmp"));
    let mut text = body.clone();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(Some(ext))
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// What `td lyrics fetch` and the daemon pass select.
#[derive(Debug, Clone, Copy, Default)]
pub struct Selection {
    /// Ignore the retry schedule: every track without lyrics.
    pub all: bool,
    /// Only external (Lidarr) tracks.
    pub external_only: bool,
    /// Re-ask the provider for every selected track and overwrite sidecars tapedeck wrote.
    pub force: bool,
    /// One track.
    pub track: Option<i64>,
    /// At most this many provider calls in this pass (the daemon's pacing).
    pub limit: Option<usize>,
}

#[derive(Serialize, Debug, Default)]
pub struct PassReport {
    pub considered: usize,
    pub found: usize,
    pub instrumental: usize,
    pub missing: usize,
    pub from_file: usize,
    pub written: usize,
    pub stopped: Option<String>,
}

impl App {
    fn audio_path(&self, t: &Track) -> Option<PathBuf> {
        t.path.as_ref().map(|p| self.config.library.join(p))
    }

    /// Lyrics for one track: the sidecar on disk, else the provider; the result goes into the
    /// database and, when allowed, next to the file. Returns the row and whether a file was written.
    async fn lyrics_one(&self, t: &Track, force: bool) -> Result<(LyricsRow, bool)> {
        let cfg = &self.config.lyrics;
        let audio = self.audio_path(t).context("track has no file")?;
        let external = is_external(t);
        let mut row = LyricsRow { track_id: t.id, fetched_at: now(), ..Default::default() };
        if !force && let Some((ext, text)) = read_sidecar(&audio) {
            row.status = "found".into();
            row.source = Some("file".into());
            if ext == "lrc" {
                row.synced = Some(text);
            } else {
                row.plain = Some(text);
            }
            self.db().lyrics_set(&row)?;
            return Ok((row, false));
        }
        let q = Query {
            title: &t.title,
            artist: &t.artist,
            album: &t.album,
            duration: t.duration_ms.map(|ms| (ms + 500) / 1000),
        };
        let found = lrclib(&q).await?;
        let mut written = false;
        match found {
            Some(f) if f.instrumental => {
                row.status = "instrumental".into();
                row.source = Some(f.source);
                row.source_id = f.source_id;
            }
            Some(f) if f.synced.is_some() || f.plain.is_some() => {
                row.status = "found".into();
                row.source = Some(f.source.clone());
                row.source_id = f.source_id.clone();
                row.synced = f.synced.clone();
                row.plain = f.plain.clone();
                let may_write = if external { cfg.external == "missing-sidecars" } else { cfg.write_sidecars };
                if may_write && let Some(ext) = write_sidecar(&audio, &f, force)? {
                    row.sidecar = Some(ext.into());
                    written = true;
                }
            }
            _ => row.status = "missing".into(),
        }
        self.db().lyrics_set(&row)?;
        Ok((row, written))
    }

    /// Fetch lyrics for `ids` (after a download) or for what `sel` picks; one provider call per
    /// second, stopping early when the provider asks to back off. Plex is told to refresh the
    /// albums whose files gained a sidecar.
    pub(crate) async fn lyrics_fetch(
        &self,
        ids: Option<&[i64]>,
        sel: Selection,
        tx: &Sender<Event>,
    ) -> Result<PassReport> {
        ensure!(self.config.lyrics.enable, "[lyrics] enable is off");
        let tracks: Vec<Track> = match (ids, sel.track) {
            (Some(ids), _) => {
                let all = self.db().all_tracks()?;
                all.into_iter().filter(|t| ids.contains(&t.id)).collect()
            }
            (None, Some(id)) => vec![self.db().track_by_id(id)?.with_context(|| format!("no track with id {id}"))?],
            (None, None) => {
                let retry_before = if sel.all || sel.force {
                    i64::MAX
                } else {
                    now() - self.config.lyrics_retry_after()?.as_secs() as i64
                };
                let tracks = self.db().lyrics_candidates(retry_before, sel.force)?;
                tracks.into_iter().filter(|t| !sel.external_only || is_external(t)).collect()
            }
        };
        let tracks: Vec<Track> = tracks.into_iter().filter(|t| t.path.is_some()).collect();
        let mut report = PassReport { considered: tracks.len(), ..Default::default() };
        let mut touched: Vec<PathBuf> = vec![];
        let mut calls = 0usize;
        for (i, t) in tracks.iter().enumerate() {
            if sel.limit.is_some_and(|l| calls >= l) {
                report.stopped = Some(format!("limit of {} provider calls reached", calls));
                break;
            }
            progress(tx, format!("lyrics {}/{}: {} - {}", i + 1, tracks.len(), t.artist, t.title)).await?;
            let had_file = !sel.force && self.audio_path(t).is_some_and(|a| read_sidecar(&a).is_some());
            if !had_file {
                if calls > 0 {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                calls += 1;
            }
            match self.lyrics_one(t, sel.force).await {
                Ok((row, written)) => {
                    match row.status.as_str() {
                        "found" if row.source.as_deref() == Some("file") => report.from_file += 1,
                        "found" => report.found += 1,
                        "instrumental" => report.instrumental += 1,
                        _ => report.missing += 1,
                    }
                    if written {
                        report.written += 1;
                        touched.extend(self.audio_path(t));
                    }
                }
                Err(e) if e.downcast_ref::<RateLimited>().is_some() => {
                    report.stopped = Some(format!("{e}"));
                    break;
                }
                Err(e) => info(tx, format!("lyrics: {} - {}: {e:#}", t.artist, t.title)).await?,
            }
        }
        if !touched.is_empty() {
            for target in &self.targets {
                if let Err(e) = target.refresh_lyrics(&touched).await {
                    info(tx, format!("{}: refresh after lyrics: {e:#}", target.name())).await?;
                }
            }
        }
        info(
            tx,
            format!(
                "lyrics: {} considered, {} found, {} from files, {} instrumental, {} missing, {} sidecars written{}",
                report.considered,
                report.found,
                report.from_file,
                report.instrumental,
                report.missing,
                report.written,
                report.stopped.as_ref().map(|s| format!(" ({s})")).unwrap_or_default()
            ),
        )
        .await?;
        Ok(report)
    }

    /// `td lyrics show`.
    pub(crate) fn lyrics_show(&self, id: i64) -> Result<Value> {
        let t = self.db().track_by_id(id)?.with_context(|| format!("no track with id {id}"))?;
        let row = self.db().lyrics_get(id)?;
        Ok(json!({
            "track": { "id": t.id, "artist": t.artist, "title": t.title, "album": t.album },
            "lyrics": row,
            "sidecar_on_disk": self.audio_path(&t).and_then(|a| read_sidecar(&a).map(|(ext, _)| ext)),
        }))
    }

    /// `td lyrics status`: coverage per library.
    pub(crate) fn lyrics_status(&self) -> Result<Value> {
        let tracks = self.db().all_tracks()?;
        let rows = self.db().lyrics_all()?;
        let by_id: std::collections::HashMap<i64, &LyricsRow> = rows.iter().map(|r| (r.track_id, r)).collect();
        let mut libs: std::collections::BTreeMap<&str, serde_json::Map<String, Value>> = Default::default();
        for t in tracks.iter().filter(|t| t.path.is_some()) {
            let lib = if is_external(t) { "external" } else { "own" };
            let e = libs.entry(lib).or_default();
            let bump = |e: &mut serde_json::Map<String, Value>, k: &str| {
                let n = e.get(k).and_then(Value::as_i64).unwrap_or(0);
                e.insert(k.into(), json!(n + 1));
            };
            bump(e, "tracks");
            match by_id.get(&t.id) {
                Some(r) => {
                    bump(e, &r.status);
                    if r.synced.is_some() {
                        bump(e, "synced");
                    }
                    if r.sidecar.is_some() || r.source.as_deref() == Some("file") {
                        bump(e, "sidecar");
                    }
                }
                None => bump(e, "untried"),
            }
        }
        Ok(json!(libs))
    }
}

/// Albums (directories) of a set of files, for a metadata refresh.
pub fn album_dirs(files: &[PathBuf]) -> BTreeSet<PathBuf> {
    files.iter().filter_map(|f| f.parent().map(Path::to_path_buf)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_names_and_reading() {
        let dir = std::env::temp_dir().join(format!("td-lyrics-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let audio = dir.join("01 - Song.opus");
        assert_eq!(sidecars(&audio)[0], dir.join("01 - Song.lrc"));
        assert!(read_sidecar(&audio).is_none());
        let f = Found { plain: Some("la la".into()), source: "lrclib".into(), ..Default::default() };
        assert_eq!(write_sidecar(&audio, &f, false).unwrap(), Some("txt"));
        assert_eq!(read_sidecar(&audio).unwrap().0, "txt");
        let f = Found { synced: Some("[00:01.00]la".into()), ..Default::default() };
        assert_eq!(write_sidecar(&audio, &f, false).unwrap(), None, "never overwrite without force");
        assert_eq!(write_sidecar(&audio, &f, true).unwrap(), Some("lrc"));
        assert_eq!(read_sidecar(&audio).unwrap().0, "lrc", "the lrc wins over the txt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lrclib_json_shape() {
        let hit: LrclibHit = serde_json::from_str(
            r#"{"id":3,"trackName":"Get Lucky","artistName":"Daft Punk","albumName":"RAM","duration":367.0,"instrumental":false,"plainLyrics":"Like the legend","syncedLyrics":"[00:01.00] Like the legend"}"#,
        )
        .unwrap();
        let f = hit.into_found();
        assert_eq!(f.source_id.as_deref(), Some("3"));
        assert!(f.synced.is_some() && f.plain.is_some() && !f.instrumental);
        let inst: LrclibHit =
            serde_json::from_str(r#"{"id":4,"instrumental":true,"plainLyrics":null,"syncedLyrics":null}"#).unwrap();
        assert!(inst.into_found().instrumental);
    }
}
