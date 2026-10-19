//! Lyrics for the library, so Plexamp, Subsonic apps and the TUI can show them. Plex reads
//! sidecar files only: `<same stem>.lrc` (synced) or `.txt` (plain) next to the audio, picked
//! up on an album metadata refresh. The source is LRCLIB (lrclib.net): open data, no token,
//! synced and plain lyrics and an "instrumental" flag. Everything found is kept in the
//! `lyrics` table, keyed `t:<track id>` for own tracks and `x:<path>` for files of external
//! libraries, so Subsonic clients and the TUI get lyrics even for files nothing may be
//! written next to (Lidarr's, by default), and a miss is retried only after
//! `[lyrics] retry_after`.
//!
//! Matching is strict, because LRCLIB's search happily returns the acoustic version of the
//! right song or the wrong length: a hit must have the same normalised title (no extra
//! "acoustic"/"live"/"remix"), an artist in common, a known length within two seconds when
//! ours is known, and synced lyrics whose last stamp lies inside the song, else the plain
//! text is used instead. Lookups try the full credit first, then the primary artist
//! ("Calvin Harris feat. Ellie Goulding" → "Calvin Harris"), with video-ish title junk removed.
//!
//! Rules: an existing sidecar wins and is indexed as source "file"; sidecars are written only
//! into tapedeck's own library unless `[lyrics] external = "missing-sidecars"`; an existing
//! sidecar is never overwritten without `--force`; audio files are never touched; a sidecar
//! tapedeck wrote follows its track (library::move_track / remove_track).

use std::{
    collections::HashSet,
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
    db::{ExternalFile, LyricsRow, Track},
    external::{is_external, norm},
    musicbrainz::{strip_feat, version_words},
    request::clean_title,
};

const LRCLIB: &str = "https://lrclib.net/api";

/// What a provider answered, after validation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Found {
    pub synced: Option<String>,
    pub plain: Option<String>,
    pub instrumental: bool,
    pub source: String,
    pub source_id: Option<String>,
    /// How it was matched: get, get-primary, search, search-primary.
    pub method: String,
    /// Provider length minus ours, when both are known.
    pub delta_ms: Option<i64>,
}

/// A song to look up: an own track or an external file.
#[derive(Debug, Clone)]
pub struct Subject {
    pub key: String,
    pub audio: PathBuf,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_ms: Option<i64>,
    pub external: bool,
}

impl Subject {
    pub fn of_track(t: &Track, library: &Path) -> Option<Subject> {
        Some(Subject {
            key: format!("t:{}", t.id),
            audio: library.join(t.path.as_ref()?),
            title: t.title.clone(),
            artist: t.artist.clone(),
            album: t.album.clone(),
            duration_ms: t.duration_ms,
            external: is_external(t),
        })
    }

    pub fn of_external(e: &ExternalFile) -> Subject {
        Subject {
            key: format!("x:{}", e.path),
            audio: PathBuf::from(&e.path),
            title: e.title.clone(),
            artist: e.artist.clone(),
            album: e.album.clone().unwrap_or_default(),
            duration_ms: e.duration_ms,
            external: true,
        }
    }

    fn duration_secs(&self) -> Option<i64> {
        self.duration_ms.map(|ms| (ms + 500) / 1000)
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct LrclibHit {
    id: Option<i64>,
    #[serde(default)]
    track_name: Option<String>,
    #[serde(default)]
    artist_name: Option<String>,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    instrumental: bool,
    plain_lyrics: Option<String>,
    synced_lyrics: Option<String>,
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

/// The provider hiccuped (5xx twice); nothing is recorded and the next pass tries again.
#[derive(Debug)]
pub struct Transient(pub String);

impl std::fmt::Display for Transient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for Transient {}

/// "Calvin Harris feat. Ellie Goulding" → "Calvin Harris"; "A & B", "A × B", "A, B" the same.
pub fn primary_artist(credit: &str) -> String {
    let lower = credit.to_lowercase();
    let mut cut = credit.len();
    for sep in
        [" feat. ", " feat ", " featuring ", " ft. ", " ft ", " with ", " × ", " x ", " & ", ", ", " vs. ", " vs "]
    {
        if let Some(i) = lower.find(sep) {
            cut = cut.min(i);
        }
    }
    credit[..cut].trim().to_string()
}

/// The title as a lyrics site would spell it: no "(Official Music Video)", no "(feat. X)".
pub fn lookup_title(title: &str) -> String {
    let cleaned = clean_title(title);
    let no_feat = strip_feat(&cleaned);
    let t = no_feat.trim();
    if t.is_empty() { title.to_string() } else { t.to_string() }
}

fn last_stamp_ms(lrc: &str) -> Option<u64> {
    subsonic_server::parse_lrc(lrc).last().map(|(ms, _)| *ms)
}

/// Why a hit was turned down, for the log.
fn reject_reason(hit: &LrclibHit, s: &Subject, strict_title: bool) -> Option<String> {
    let want_title = norm(&lookup_title(&s.title));
    if strict_title && let Some(got) = &hit.track_name {
        let got_title = norm(&lookup_title(got));
        if got_title != want_title {
            return Some(format!("title {got:?}"));
        }
        let ours: HashSet<&str> = version_words(&s.title).into_iter().collect();
        if let Some(extra) = version_words(got).into_iter().find(|w| !ours.contains(w)) {
            return Some(format!("a {extra} version"));
        }
    }
    if strict_title && let Some(got) = &hit.artist_name {
        let (a, b) = (norm(&primary_artist(&s.artist)), norm(got));
        if !a.is_empty() && !b.contains(&a) && !a.contains(&b) {
            return Some(format!("artist {got:?}"));
        }
    }
    if let Some(want) = s.duration_secs() {
        match hit.duration {
            None => return Some("no length".into()),
            Some(got) if (got - want as f64).abs() > 2.0 => return Some(format!("length {got:.0}s vs {want}s")),
            _ => {}
        }
    }
    None
}

/// Turn an accepted hit into what we keep; synced lyrics that run past the end of the song
/// are dropped in favour of the plain text.
fn accept(hit: LrclibHit, s: &Subject, method: &str) -> Found {
    let clean = |v: Option<String>| v.filter(|v| !v.trim().is_empty());
    let delta_ms = match (hit.duration, s.duration_ms) {
        (Some(got), Some(want)) => Some((got * 1000.0) as i64 - want),
        _ => None,
    };
    let mut synced = clean(hit.synced_lyrics);
    if let (Some(lrc), Some(ms)) = (&synced, s.duration_ms)
        && last_stamp_ms(lrc).is_some_and(|last| last as i64 > ms + 2000)
    {
        synced = None;
    }
    Found {
        synced,
        plain: clean(hit.plain_lyrics),
        instrumental: hit.instrumental,
        source: "lrclib".into(),
        source_id: hit.id.map(|i| i.to_string()),
        method: method.into(),
        delta_ms,
    }
}

fn retry_after(res: &reqwest::Response) -> Duration {
    res.headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(60))
}

/// One `GET`, with a 429 turned into `RateLimited`. `Ok(None)` is 404.
async fn lrclib_get(path: &str, pairs: &[(&str, String)]) -> Result<Option<reqwest::Response>> {
    let mut url = reqwest::Url::parse(&format!("{LRCLIB}/{path}"))?;
    for (k, v) in pairs {
        url.query_pairs_mut().append_pair(k, v);
    }
    let res = crate::http().get(url).send().await.with_context(|| format!("lrclib {path}"))?;
    match res.status() {
        s if s.is_success() => Ok(Some(res)),
        reqwest::StatusCode::NOT_FOUND => Ok(None),
        reqwest::StatusCode::TOO_MANY_REQUESTS => bail!(RateLimited(retry_after(&res))),
        s => bail!("lrclib {path}: HTTP {s}"),
    }
}

/// Ask LRCLIB for `s`, skipping `rejected` ids: `get` with the full credit, search, then both
/// again with the primary artist. `Ok(None)` is a miss, remembered until `retry_after`.
pub async fn lrclib(s: &Subject, rejected: &[String]) -> Result<Option<Found>> {
    let title = lookup_title(&s.title);
    let primary = primary_artist(&s.artist);
    let mut artists = vec![(s.artist.clone(), "")];
    if norm(&primary) != norm(&s.artist) {
        artists.push((primary, "-primary"));
    }
    let dur = s.duration_secs().map(|d| d.to_string());
    let not_rejected = |h: &LrclibHit| !h.id.is_some_and(|i| rejected.contains(&i.to_string()));
    for (artist, suffix) in &artists {
        // `get` answers 5xx when its own external lookup for an unknown song fails, so a failure
        // here just means "not cached", and the search decides.
        let mut pairs =
            vec![("artist_name", artist.clone()), ("track_name", title.clone()), ("album_name", s.album.clone())];
        if let Some(d) = &dur {
            pairs.push(("duration", d.clone()));
        }
        if let Ok(Some(res)) = lrclib_get("get", &pairs).await
            && let Ok(hit) = res.json::<LrclibHit>().await
            && not_rejected(&hit)
            && reject_reason(&hit, s, false).is_none()
        {
            return Ok(Some(accept(hit, s, &format!("get{suffix}"))));
        }
        // Search: by title and artist, two tries on a 5xx (LRCLIB is flaky rather than down).
        let pairs = [("track_name", title.clone()), ("artist_name", artist.clone())];
        let mut hits: Option<Vec<LrclibHit>> = None;
        for attempt in 0..2 {
            match lrclib_get("search", &pairs).await {
                Ok(Some(res)) => {
                    hits = Some(res.json().await.unwrap_or_default());
                    break;
                }
                Ok(None) => {
                    hits = Some(vec![]);
                    break;
                }
                Err(e) if e.downcast_ref::<RateLimited>().is_some() => return Err(e),
                Err(e) if attempt == 0 => {
                    tracing::debug!("lrclib search: {e:#}; retrying once");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                Err(e) => bail!(Transient(format!("{e:#}"))),
            }
        }
        let hits = hits.unwrap_or_default();
        let mut best: Option<(LrclibHit, (bool, bool))> = None;
        for hit in hits.into_iter().filter(not_rejected) {
            match reject_reason(&hit, s, true) {
                Some(why) => tracing::debug!("lrclib: {} - {}: skipped {:?}: {why}", s.artist, s.title, hit.id),
                None => {
                    let score = (hit.synced_lyrics.is_some(), hit.plain_lyrics.is_some());
                    if best.as_ref().is_none_or(|(_, b)| score > *b) {
                        best = Some((hit, score));
                    }
                }
            }
        }
        if let Some((hit, _)) = best {
            return Ok(Some(accept(hit, s, &format!("search{suffix}"))));
        }
    }
    Ok(None)
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
    /// Ignore the retry schedule: every song without lyrics.
    pub all: bool,
    /// Only external files.
    pub external_only: bool,
    /// Re-ask the provider for every selected song and overwrite sidecars.
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
    /// Provider hiccups; these songs stay untried and the next pass takes them again.
    pub errors: usize,
    pub stopped: Option<String>,
}

impl App {
    /// Every song with a file: own tracks (keyed `t:`) and external files (`x:`) that no track
    /// already stands for.
    pub(crate) fn lyrics_subjects(&self) -> Result<Vec<Subject>> {
        let tracks = self.db().all_tracks()?;
        let externals = self.db().external_all()?;
        let lib = &self.config.library;
        let mut out: Vec<Subject> = tracks
            .iter()
            .filter(|t| t.path.is_some() && matches!(t.status.as_str(), "ready" | "external"))
            .filter_map(|t| Subject::of_track(t, lib))
            .collect();
        let taken: HashSet<PathBuf> = out.iter().map(|s| s.audio.clone()).collect();
        out.extend(externals.iter().filter(|e| !taken.contains(Path::new(&e.path))).map(Subject::of_external));
        Ok(out)
    }

    fn rejected_ids(&self, key: &str) -> Result<Vec<String>> {
        Ok(self
            .db()
            .meta_get(&format!("lyrics.rejected.{key}"))?
            .map(|v| v.split(',').filter(|s| !s.is_empty()).map(str::to_string).collect())
            .unwrap_or_default())
    }

    /// Lyrics for one song: the sidecar on disk, else the provider; the result goes into the
    /// database and, when allowed, next to the file. Returns the row and whether a file was written.
    async fn lyrics_one(&self, s: &Subject, force: bool) -> Result<(LyricsRow, bool)> {
        let cfg = &self.config.lyrics;
        let mut row = LyricsRow { key: s.key.clone(), fetched_at: now(), ..Default::default() };
        if !force && let Some((ext, text)) = read_sidecar(&s.audio) {
            row.status = "found".into();
            row.source = Some("file".into());
            row.method = Some("file".into());
            if ext == "lrc" {
                row.synced = Some(text);
            } else {
                row.plain = Some(text);
            }
            self.db().lyrics_set(&row)?;
            return Ok((row, false));
        }
        let rejected = self.rejected_ids(&s.key)?;
        let found = lrclib(s, &rejected).await?;
        let mut written = false;
        match found {
            Some(f) if f.instrumental => {
                row.status = "instrumental".into();
                row.source = Some(f.source);
                row.source_id = f.source_id;
                row.method = Some(f.method);
            }
            Some(f) if f.synced.is_some() || f.plain.is_some() => {
                row.status = "found".into();
                row.source = Some(f.source.clone());
                row.source_id = f.source_id.clone();
                row.method = Some(f.method.clone());
                row.delta_ms = f.delta_ms;
                row.synced = f.synced.clone();
                row.plain = f.plain.clone();
                let may_write = if s.external { cfg.external == "missing-sidecars" } else { cfg.write_sidecars };
                if may_write && let Some(ext) = write_sidecar(&s.audio, &f, force)? {
                    row.sidecar = Some(ext.into());
                    written = true;
                }
            }
            _ => row.status = "missing".into(),
        }
        self.db().lyrics_set(&row)?;
        Ok((row, written))
    }

    /// Fetch lyrics for `keys` (after a download) or for what `sel` picks; one provider call per
    /// second, stopping early when the provider asks to back off. Plex is told to refresh the
    /// albums whose files gained a sidecar.
    pub(crate) async fn lyrics_fetch(
        &self,
        keys: Option<&[String]>,
        sel: Selection,
        tx: &Sender<Event>,
    ) -> Result<PassReport> {
        ensure!(self.config.lyrics.enable, "[lyrics] enable is off");
        let all = self.lyrics_subjects()?;
        let subjects: Vec<Subject> = match (keys, sel.track) {
            (Some(keys), _) => all.into_iter().filter(|s| keys.contains(&s.key)).collect(),
            (None, Some(id)) => {
                let key = format!("t:{id}");
                let one = all
                    .into_iter()
                    .find(|s| s.key == key)
                    .with_context(|| format!("no track with id {id} and a file"))?;
                vec![one]
            }
            (None, None) => {
                let rows = self.db().lyrics_all()?;
                let retry_before =
                    if sel.all { i64::MAX } else { now() - self.config.lyrics_retry_after()?.as_secs() as i64 };
                let due = |s: &Subject| match rows.iter().find(|r| r.key == s.key) {
                    None => true,
                    Some(r) => sel.force || (r.status == "missing" && r.fetched_at < retry_before),
                };
                all.into_iter().filter(|s| (!sel.external_only || s.external) && due(s)).collect()
            }
        };
        let mut report = PassReport { considered: subjects.len(), ..Default::default() };
        let mut touched: Vec<PathBuf> = vec![];
        let mut calls = 0usize;
        for (i, s) in subjects.iter().enumerate() {
            if sel.limit.is_some_and(|l| calls >= l) {
                report.stopped = Some(format!("limit of {calls} provider calls reached"));
                break;
            }
            progress(tx, format!("lyrics {}/{}: {} - {}", i + 1, subjects.len(), s.artist, s.title)).await?;
            let had_file = !sel.force && read_sidecar(&s.audio).is_some();
            if !had_file {
                if calls > 0 {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                calls += 1;
            }
            match self.lyrics_one(s, sel.force).await {
                Ok((row, written)) => {
                    match row.status.as_str() {
                        "found" if row.source.as_deref() == Some("file") => report.from_file += 1,
                        "found" => report.found += 1,
                        "instrumental" => report.instrumental += 1,
                        _ => report.missing += 1,
                    }
                    if written {
                        report.written += 1;
                        touched.push(s.audio.clone());
                    }
                }
                Err(e) if e.downcast_ref::<RateLimited>().is_some() => {
                    report.stopped = Some(format!("{e}"));
                    break;
                }
                Err(e) => {
                    report.errors += 1;
                    info(tx, format!("lyrics: {} - {}: {e:#}", s.artist, s.title)).await?;
                }
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
                "lyrics: {} considered, {} found, {} from files, {} instrumental, {} missing, {} errors, {} sidecars written{}",
                report.considered,
                report.found,
                report.from_file,
                report.instrumental,
                report.missing,
                report.errors,
                report.written,
                report.stopped.as_ref().map(|s| format!(" ({s})")).unwrap_or_default()
            ),
        )
        .await?;
        Ok(report)
    }

    fn subject_of_track(&self, id: i64) -> Result<Subject> {
        let t = self.db().track_by_id(id)?.with_context(|| format!("no track with id {id}"))?;
        Subject::of_track(&t, &self.config.library).with_context(|| format!("track {id} has no file"))
    }

    /// `td lyrics show`: flat, so the human rendering shows every field.
    pub(crate) fn lyrics_show(&self, id: i64) -> Result<Value> {
        let s = self.subject_of_track(id)?;
        let row = self.db().lyrics_get(&s.key)?;
        let on_disk = read_sidecar(&s.audio).map(|(ext, _)| ext);
        let mut v =
            json!({ "id": id, "artist": s.artist, "title": s.title, "album": s.album, "sidecar_on_disk": on_disk });
        if let Some(r) = row {
            v["status"] = json!(r.status);
            v["source"] = json!(r.source);
            v["source_id"] = json!(r.source_id);
            v["method"] = json!(r.method);
            v["delta_ms"] = json!(r.delta_ms);
            v["sidecar_written"] = json!(r.sidecar);
            v["fetched_at"] = json!(r.fetched_at);
            v["last_stamp_ms"] = json!(r.synced.as_deref().and_then(last_stamp_ms));
            v["duration_ms"] = json!(s.duration_ms);
            v["lyrics"] = json!(r.synced.or(r.plain));
        } else {
            v["status"] = json!("untried");
        }
        Ok(v)
    }

    /// `td lyrics status`: coverage per library, and how the found ones were matched.
    pub(crate) fn lyrics_status(&self) -> Result<Value> {
        let subjects = self.lyrics_subjects()?;
        let rows = self.db().lyrics_all()?;
        let by_key: std::collections::HashMap<&str, &LyricsRow> = rows.iter().map(|r| (r.key.as_str(), r)).collect();
        let mut libs: std::collections::BTreeMap<&str, serde_json::Map<String, Value>> = Default::default();
        let bump = |e: &mut serde_json::Map<String, Value>, k: &str| {
            let n = e.get(k).and_then(Value::as_i64).unwrap_or(0);
            e.insert(k.into(), json!(n + 1));
        };
        for s in &subjects {
            let e = libs.entry(if s.external { "external" } else { "own" }).or_default();
            bump(e, "songs");
            match by_key.get(s.key.as_str()) {
                Some(r) => {
                    bump(e, &r.status);
                    if r.synced.is_some() {
                        bump(e, "synced");
                    }
                    if r.sidecar.is_some() || r.source.as_deref() == Some("file") {
                        bump(e, "sidecar");
                    }
                    if let Some(m) = &r.method {
                        bump(e, &format!("by_{m}"));
                    }
                }
                None => bump(e, "untried"),
            }
        }
        Ok(json!(libs))
    }

    /// `td lyrics reject`: these lyrics are wrong. The sidecar tapedeck wrote goes, the source
    /// id is blocklisted for this song, and the next candidate is tried right away.
    pub(crate) async fn lyrics_reject(&self, id: i64, tx: &Sender<Event>) -> Result<Value> {
        let s = self.subject_of_track(id)?;
        let row = self.db().lyrics_get(&s.key)?.with_context(|| format!("track {id} has no lyrics to reject"))?;
        ensure!(
            row.sidecar.is_some() || row.source.as_deref() != Some("file"),
            "track {id}'s lyrics come from a file tapedeck did not write; delete it yourself"
        );
        if let Some(ext) = &row.sidecar {
            let path = s.audio.with_extension(ext);
            if path.is_file() {
                std::fs::remove_file(&path).with_context(|| format!("deleting {}", path.display()))?;
                info(tx, format!("deleted {}", path.display())).await?;
            }
        }
        if let Some(sid) = &row.source_id {
            let key = format!("lyrics.rejected.{}", s.key);
            let mut ids = self.rejected_ids(&s.key)?;
            if !ids.contains(sid) {
                ids.push(sid.clone());
            }
            self.db().meta_set(&key, &ids.join(","))?;
        }
        self.db().lyrics_delete(&s.key)?;
        let (row, written) = self.lyrics_one(&s, true).await?;
        if written {
            for target in &self.targets {
                let _ = target.refresh_lyrics(std::slice::from_ref(&s.audio)).await;
            }
        }
        Ok(
            json!({ "id": id, "status": row.status, "source_id": row.source_id, "method": row.method, "sidecar_written": written }),
        )
    }

    /// `td lyrics check`: synced lyrics tapedeck fetched whose last stamp lies past the end of
    /// the song are wrong; with `fix` they are rejected and refetched, otherwise only listed.
    pub(crate) async fn lyrics_check(&self, fix: bool, tx: &Sender<Event>) -> Result<Value> {
        let subjects = self.lyrics_subjects()?;
        let rows = self.db().lyrics_all()?;
        let mut suspect = vec![];
        for r in rows.iter().filter(|r| r.source.as_deref() == Some("lrclib")) {
            let Some(s) = subjects.iter().find(|s| s.key == r.key) else { continue };
            let (Some(lrc), Some(ms)) = (&r.synced, s.duration_ms) else { continue };
            if let Some(last) = last_stamp_ms(lrc)
                && last as i64 > ms + 2000
            {
                suspect.push(json!({ "key": r.key, "artist": s.artist, "title": s.title, "source_id": r.source_id, "last_stamp_ms": last, "duration_ms": ms }));
            }
        }
        if fix {
            for v in &suspect {
                let key = v["key"].as_str().unwrap_or("");
                if let Some(id) = key.strip_prefix("t:").and_then(|i| i.parse::<i64>().ok()) {
                    match self.lyrics_reject(id, tx).await {
                        Ok(r) => {
                            info(tx, format!("lyrics: refetched {} - {}: {}", v["artist"], v["title"], r["status"]))
                                .await?
                        }
                        Err(e) => info(tx, format!("lyrics: {} - {}: {e:#}", v["artist"], v["title"])).await?,
                    }
                }
            }
        }
        Ok(json!({ "suspect": suspect.len(), "fixed": fix, "items": suspect }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credits_and_titles_are_simplified_for_lookup() {
        assert_eq!(primary_artist("Calvin Harris feat. Ellie Goulding"), "Calvin Harris");
        assert_eq!(primary_artist("A & B"), "A");
        assert_eq!(primary_artist("YOASOBI"), "YOASOBI");
        assert_eq!(lookup_title("アイドル (Official Music Video)"), "アイドル");
        assert_eq!(lookup_title("Outside (feat. Ellie Goulding)"), "Outside");
        assert_eq!(lookup_title("Don't Stop the Music"), "Don't Stop the Music");
    }

    fn subject(title: &str, artist: &str, secs: i64) -> Subject {
        Subject {
            key: "t:1".into(),
            audio: PathBuf::from("/x.opus"),
            title: title.into(),
            artist: artist.into(),
            album: "A".into(),
            duration_ms: Some(secs * 1000),
            external: false,
        }
    }

    fn hit(title: &str, artist: &str, dur: Option<f64>) -> LrclibHit {
        LrclibHit {
            id: Some(1),
            track_name: Some(title.into()),
            artist_name: Some(artist.into()),
            duration: dur,
            instrumental: false,
            plain_lyrics: Some("la".into()),
            synced_lyrics: None,
        }
    }

    #[test]
    fn search_hits_are_checked_strictly() {
        let s = subject("Without Me", "Dayseeker", 225);
        assert!(reject_reason(&hit("Without Me", "Dayseeker", Some(226.0)), &s, true).is_none());
        assert!(
            reject_reason(&hit("Without Me (acoustic)", "Dayseeker", Some(225.0)), &s, true)
                .unwrap()
                .contains("acoustic")
        );
        assert!(reject_reason(&hit("Without Me", "Dayseeker", None), &s, true).unwrap().contains("length"));
        assert!(reject_reason(&hit("Without Me", "Dayseeker", Some(240.0)), &s, true).unwrap().contains("length"));
        assert!(reject_reason(&hit("Without You", "Dayseeker", Some(225.0)), &s, true).unwrap().contains("title"));
        assert!(reject_reason(&hit("Without Me", "Halsey", Some(225.0)), &s, true).unwrap().contains("artist"));
        let feat = subject("Outside", "Calvin Harris feat. Ellie Goulding", 227);
        assert!(
            reject_reason(&hit("Outside", "Calvin Harris", Some(226.0)), &feat, true).is_none(),
            "primary artist is enough"
        );
        // `get` results skip the title/artist check but not the length.
        assert!(reject_reason(&hit("whatever", "whoever", Some(226.0)), &s, false).is_none());
    }

    #[test]
    fn synced_lyrics_past_the_end_are_dropped() {
        let s = subject("Get Lucky", "Daft Punk", 248);
        let mut h = hit("Get Lucky", "Daft Punk", Some(249.5));
        h.synced_lyrics = Some("[00:10.00]a\n[05:16.00]b".into());
        let f = accept(h, &s, "get");
        assert!(f.synced.is_none() && f.plain.is_some());
        assert_eq!(f.delta_ms, Some(1500));
    }

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
}
