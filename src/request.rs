//! Song requests: a link or "Artist - Title" comes in (Telegram, `td request`), tapedeck works
//! out which recording it is, fetches the best audio for it, files it, and puts it on this
//! year's "Requests" playlist. It asks only when it is not sure: a cover by someone else, a
//! remix the recording doesn't carry, no MusicBrainz match at all.
//!
//! A YouTube playlist (`td import <link>`) is the same, once per video: each entry is matched
//! to a MusicBrainz recording and downloaded as album audio through the normal path, never as
//! the video itself (playlists are mostly music videos, with intros and skits). Unsure entries
//! are listed and only taken with `--also`. Mixes (endless `RD…` lists) and private lists
//! (Liked, Watch later: they need a signed-in account) are refused.

use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::mpsc::Sender;

use crate::{
    App, Event,
    app::{display_name, info, new_track, progress_at},
    db::Track,
    external::{chrono_now, find_external, present},
    library,
    musicbrainz::{Candidate, Recording, strip_feat, version_words},
};

/// What the link (or text) says about itself.
#[derive(Serialize, Clone, Debug, Default)]
pub struct Source {
    pub url: Option<String>,
    pub video_id: Option<String>,
    pub raw_title: String,
    pub channel: Option<String>,
    pub artist: String,
    pub title: String,
    pub duration_ms: Option<i64>,
    pub year: Option<i64>,
    /// A YouTube Music / "Topic" track: the length is the song's, not a video's.
    pub music: bool,
}

#[derive(Serialize, Clone, Debug)]
pub struct Analysis {
    pub source: Source,
    /// Best first.
    pub candidates: Vec<Candidate>,
    /// True when the top candidate can be taken without asking.
    pub sure: bool,
    pub reason: String,
    /// The library already has the top candidate.
    pub existing: Option<Track>,
}

pub enum Choice {
    /// Index into `candidates`.
    Candidate(usize),
    /// Any recording id, e.g. after a correction.
    Recording(String),
    /// Download the sent link as it is, tagged from its own metadata.
    ThisVideo,
}

#[derive(Serialize, Clone, Debug)]
pub struct Outcome {
    pub track: Track,
    pub playlist: String,
    pub already_had: bool,
}

/// Words that mean "this is the song, not a different take", safe to strip from a video title.
const JUNK: &[&str] = &[
    "official",
    "video",
    "lyric",
    "lyrics",
    "audio",
    "visualizer",
    "visualiser",
    "4k",
    "hd",
    "hq",
    "upgrade",
    "remaster",
    "remastered",
    "clip",
    "premiere",
    "mv",
    "m/v",
    "performance",
];

/// "Numb (Official Music Video) [4K UPGRADE] – Linkin Park" → "Numb – Linkin Park".
pub fn clean_title(raw: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    let mut group = String::new();
    for c in raw.chars() {
        match c {
            '(' | '[' => {
                depth += 1;
                if depth == 1 {
                    group.clear();
                }
            }
            ')' | ']' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    let g = group.to_lowercase();
                    let junk = g.split(|c: char| !c.is_alphanumeric() && c != '/').any(|w| JUNK.contains(&w));
                    if !junk {
                        out.push_str(&format!("({})", group.trim()));
                    }
                }
            }
            _ if depth > 0 => group.push(c),
            _ => out.push(c),
        }
    }
    // "Title | Artist" and "Title // Artist" are separators too; trailing "HD"-style words go.
    let out = out.replace(" | ", " - ").replace(" // ", " - ");
    out.split_whitespace().filter(|w| !JUNK.contains(&w.to_lowercase().as_str())).collect::<Vec<_>>().join(" ")
}

/// Split "A - B" (or en/em dash) into (artist, title); the channel name decides which side is
/// the artist when it matches one of them.
pub fn split_artist_title(clean: &str, channel: Option<&str>) -> (String, String) {
    let parts: Vec<&str> =
        clean.splitn(2, [" - ", " – ", " — "].iter().find(|s| clean.contains(*s)).copied().unwrap_or(" - ")).collect();
    let chan = channel.map(channel_artist).unwrap_or_default().to_lowercase();
    match parts.as_slice() {
        [a, b] => {
            let (a, b) = (a.trim(), b.trim());
            if !chan.is_empty() && b.to_lowercase() == chan && a.to_lowercase() != chan {
                (b.to_string(), a.to_string())
            } else {
                (a.to_string(), b.to_string())
            }
        }
        _ => (channel.map(channel_artist).unwrap_or_default(), clean.trim().to_string()),
    }
}

/// "Linkin Park - Topic" → "Linkin Park", "ArianaGrandeVevo" → "ArianaGrande".
fn channel_artist(channel: &str) -> String {
    let c = channel.trim();
    let c = c.strip_suffix(" - Topic").unwrap_or(c);
    let lower = c.to_lowercase();
    let c = if lower.ends_with("vevo") { &c[..c.len() - 4] } else { c };
    c.trim_end_matches([' ', '-']).to_string()
}

fn norm(s: &str) -> String {
    strip_feat(s)
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn looks_like_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

impl App {
    /// What was sent, and what MusicBrainz thinks it is.
    pub(crate) async fn analyze_request(&self, input: &str) -> Result<Analysis> {
        let input = input.trim();
        ensure!(!input.is_empty(), "nothing to look up");
        let source = if looks_like_url(input) {
            let v = self.downloader.probe(input).await?;
            source_from_video(input, &v)
        } else {
            let (artist, title) = split_artist_title(input, None);
            ensure!(!title.is_empty(), "send a link or \"Artist - Title\"");
            Source { raw_title: input.to_string(), artist, title, ..Default::default() }
        };
        let length = if source.music { source.duration_ms } else { None };
        let candidates = self.mb.search_candidates(&source.artist, &source.title, length).await?;
        let (sure, reason) = judge(&source, &candidates);
        let existing = match candidates.first() {
            Some(c) => self.db().track_by_mbid(&c.mbid)?.filter(present),
            None => None,
        };
        Ok(Analysis { source, candidates, sure, reason, existing })
    }

    /// Get the chosen recording into the library and onto this year's Requests playlist.
    pub(crate) async fn fulfil_request(&self, a: &Analysis, choice: Choice, tx: &Sender<Event>) -> Result<Outcome> {
        let cfg = &self.config;
        let this_video = matches!(choice, Choice::ThisVideo);
        let (mbid, r) = match choice {
            Choice::Candidate(i) => {
                let c = a.candidates.get(i).context("no such candidate")?;
                (c.mbid.clone(), self.mb.recording(&c.mbid).await?)
            }
            Choice::Recording(m) => {
                ensure!(library::is_uuid(&m), "{m:?} is not a MusicBrainz id");
                (m.clone(), self.mb.recording(&m).await?)
            }
            Choice::ThisVideo => {
                let id = a.source.video_id.clone().context("that was not a link; pick a recording instead")?;
                (format!("youtube:{id}"), video_recording(&a.source))
            }
        };
        let id = self.db().insert_track(&new_track(&mbid, &r))?;
        let mut t = self.db().track_by_id(id)?.context("track vanished")?;
        let already_had = present(&t);
        if !already_had {
            let external = self.db().external_all()?;
            if let Some(e) = find_external(&external, &t) {
                self.db().mark_external(id, &e.path)?;
                info(tx, format!("external has {} - {}: {}", t.artist, t.title, e.path)).await?;
            } else {
                if this_video {
                    // Exactly this link, as sent.
                    self.db().set_source_url(id, a.source.url.as_deref().context("no link")?)?;
                    t = self.db().track_by_id(id)?.context("track vanished")?;
                }
                info(tx, format!("downloading {} - {}", t.artist, t.title)).await?;
                let got = match self.downloader.download(&t, &cfg.tmp_dir()).await {
                    Ok(got) => got,
                    // The best source can be refused (403) or missing; the link itself is the fallback.
                    Err(e) if a.source.url.is_some() && t.source_url.is_none() => {
                        info(tx, format!("search download failed ({e:#}); taking the link's audio")).await?;
                        self.db().set_source_url(id, a.source.url.as_deref().unwrap_or_default())?;
                        t = self.db().track_by_id(id)?.context("track vanished")?;
                        self.downloader.download(&t, &cfg.tmp_dir()).await?
                    }
                    Err(e) => {
                        self.db().mark_failed(id, &format!("{e:#}"))?;
                        return Err(e);
                    }
                };
                let rel = library::file_track(&cfg.library, &cfg.path_template, &t, &got.path, false)?;
                self.db().mark_ready(id, &rel.to_string_lossy(), got.source_url.as_deref())?;
            }
        }
        let track = self.db().track_by_id(id)?.context("track vanished")?;
        self.db().record_request(id, a.source.url.as_deref().or(Some(a.source.raw_title.as_str())))?;
        let playlist = self.add_to_requests(id, tx).await?;
        Ok(Outcome { track, playlist, already_had })
    }

    /// The requester says the pick was right.
    pub(crate) fn confirm_request(&self, track_id: i64) -> Result<()> {
        self.db().confirm_request(track_id)
    }

    /// Drop a requested track again: file, row, playlist entry, push.
    pub(crate) async fn undo_request(&self, track_id: i64, tx: &Sender<Event>) -> Result<()> {
        let t = self.db().track_by_id(track_id)?.with_context(|| format!("no track with id {track_id}"))?;
        let playlists = self.db().playlists_with_track(track_id)?;
        if let Some(rel) = &t.path
            && t.status == "ready"
            && self.db().tracks_with_path(rel)?.len() == 1
        {
            library::remove_track(&self.config.library, Path::new(rel))?;
        }
        self.db().delete_track(track_id)?;
        for pid in playlists {
            self.publish_playlist(pid, tx).await?;
        }
        info(tx, format!("removed {} - {}", t.artist, t.title)).await
    }

    /// Prepend to "Requests <year>" (its own playlist kind, so it gets its own m3u8 and Plex
    /// playlist and the year's history never grows unbounded), write it, push it.
    async fn add_to_requests(&self, track_id: i64, tx: &Sender<Event>) -> Result<String> {
        let today = chrono_now();
        let year = &today[..4];
        let kind = format!("requests-{year}");
        let name = format!("Requests {year}");
        let pid = self.db().upsert_playlist(&kind, &name, &kind, &today[..10])?;
        let mut ids: Vec<i64> = vec![track_id];
        ids.extend(self.db().playlist_tracks(pid)?.iter().map(|t| t.id).filter(|id| *id != track_id));
        self.db().set_playlist_tracks(pid, &ids)?;
        self.publish_playlist(pid, tx).await?;
        Ok(display_name(&kind))
    }
}

/// The `list=` id of a playlist link, refusing the kinds yt-dlp cannot read without an account
/// or that never end.
pub fn playlist_id(url: &str) -> Result<String> {
    let id = url
        .split(['?', '&'])
        .find_map(|kv| kv.strip_prefix("list="))
        .filter(|id| !id.is_empty())
        .with_context(|| format!("{url} is not a playlist link (no list=)"))?;
    if id.starts_with("RD") {
        bail!("{id} is a mix: YouTube makes those up as you go and they never end");
    }
    if matches!(id, "LL" | "LM" | "WL") {
        bail!("{id} is a private list (liked music, watch later); it needs a signed-in account");
    }
    Ok(id.to_string())
}

/// One playlist video and what MusicBrainz made of it.
#[derive(Clone, Debug)]
pub struct PlaylistEntry {
    /// 1-based, as in YouTube.
    pub pos: usize,
    pub source: Source,
    pub candidates: Vec<Candidate>,
    pub sure: bool,
    pub reason: String,
    /// The library already has the top candidate.
    pub have: bool,
}

/// A playlist read and matched, not yet imported.
#[derive(Clone, Debug)]
pub struct PlaylistPlan {
    pub id: String,
    pub name: String,
    pub entries: Vec<PlaylistEntry>,
}

impl PlaylistPlan {
    pub fn unsure(&self) -> impl Iterator<Item = &PlaylistEntry> {
        self.entries.iter().filter(|e| !e.sure)
    }
}

/// How an import went: tracks on the playlist, downloads, and the failures by name.
#[derive(Debug, Default)]
pub struct ImportReport {
    pub tracks: usize,
    pub downloaded: usize,
    pub failed: Vec<String>,
}

impl App {
    /// Read a playlist and match every video to a recording. Nothing is written.
    pub(crate) async fn analyze_playlist(&self, url: &str, tx: &Sender<Event>) -> Result<PlaylistPlan> {
        let id = playlist_id(url)?;
        let pl = self.downloader.playlist(&format!("https://www.youtube.com/playlist?list={id}")).await?;
        let name = pl["title"].as_str().unwrap_or(&id).trim_start_matches("Album - ").to_string();
        let owner = pl["channel"].as_str().or(pl["uploader"].as_str()).map(|s| s.to_lowercase());
        let videos: Vec<&Value> = pl["entries"].as_array().map(|e| e.iter().collect()).unwrap_or_default();
        ensure!(!videos.is_empty(), "{name:?} has no videos (or is private)");
        info(tx, format!("{name}: {} videos", videos.len())).await?;
        let mut entries = vec![];
        for (i, v) in videos.iter().enumerate() {
            let pos = i + 1;
            let Some(vurl) = v["url"]
                .as_str()
                .map(str::to_string)
                .or_else(|| v["id"].as_str().map(|v| format!("https://www.youtube.com/watch?v={v}")))
            else {
                continue;
            };
            let mut source = source_from_video(&vurl, v);
            progress_at(tx, pos, videos.len(), format!("{} - {}", source.artist, source.title)).await?;
            // The curator's own uploads (a label's playlist of its catalogue) are as good as the
            // artist's: don't let the uploader check doubt them.
            if source.channel.as_deref().map(str::to_lowercase) == owner {
                source.channel = None;
            }
            let length = if source.music { source.duration_ms } else { None };
            let candidates = match self.mb.search_candidates(&source.artist, &source.title, length).await {
                Ok(c) => c,
                Err(e) => {
                    info(tx, format!("{pos}: musicbrainz: {e:#}")).await?;
                    vec![]
                }
            };
            let (sure, reason) = judge(&source, &candidates);
            let have = match candidates.first() {
                Some(c) => {
                    let t = self.db().track_by_mbid(&c.mbid)?;
                    t.filter(present).is_some()
                }
                None => false,
            };
            entries.push(PlaylistEntry { pos, source, candidates, sure, reason, have });
        }
        Ok(PlaylistPlan { id, name, entries })
    }

    /// Import a plan: the sure entries' top match plus `picks` (position, candidate index) for
    /// unsure ones, in YouTube's order; download what is missing; write and push the playlist.
    pub(crate) async fn apply_playlist(
        &self,
        plan: &PlaylistPlan,
        picks: &[(usize, usize)],
        tx: &Sender<Event>,
    ) -> Result<ImportReport> {
        let mut ids = vec![];
        for e in &plan.entries {
            let ci = if e.sure { Some(0) } else { picks.iter().find(|(p, _)| *p == e.pos).map(|(_, c)| *c) };
            let Some(c) = ci.and_then(|ci| e.candidates.get(ci)) else { continue };
            // Bind first: the DB guard must not live across the await below.
            let existing = self.db().track_by_mbid(&c.mbid)?;
            let tid = match existing {
                Some(t) => t.id,
                None => match self.mb.recording(&c.mbid).await {
                    Ok(r) => self.db().insert_track(&new_track(&c.mbid, &r))?,
                    Err(err) => {
                        info(tx, format!("{}: {} - {}: {err:#}", e.pos, c.artist, c.title)).await?;
                        continue;
                    }
                },
            };
            if !ids.contains(&tid) {
                ids.push(tid);
            }
        }
        let kind = format!("youtube-{}", plan.id);
        let pid = self.db().upsert_playlist(&kind, &plan.name, &kind, &chrono_now()[..10])?;
        self.db().set_playlist_tracks(pid, &ids)?;
        info(tx, format!("{}: {} tracks; downloading what is missing", plan.name, ids.len())).await?;
        let before: Vec<i64> = self.db().playlist_tracks(pid)?.iter().filter(|t| !present(t)).map(|t| t.id).collect();
        // The usual download step (external libraries first, lyrics after), then this playlist.
        let downloaded = self.sync(None, true, true, tx).await;
        self.publish_playlist(pid, tx).await?;
        let after = self.db().playlist_tracks(pid)?;
        let report = ImportReport {
            tracks: ids.len(),
            downloaded: after.iter().filter(|t| before.contains(&t.id) && present(t)).count(),
            failed: after
                .iter()
                .filter(|t| before.contains(&t.id) && !present(t))
                .map(|t| format!("{} - {}", t.artist, t.title))
                .collect(),
        };
        // Failures elsewhere in the queue are not this import's; report ours, not the sync's.
        if let Err(e) = downloaded
            && report.failed.is_empty()
        {
            tracing::debug!("import: other downloads failed: {e:#}");
        }
        Ok(report)
    }

    /// `td import <link>`: the dry run, or with `apply` the import (`also` takes the top match
    /// of those unsure positions too).
    pub(crate) async fn import_playlist(
        &self,
        url: &str,
        apply: bool,
        also: &[usize],
        tx: &Sender<Event>,
    ) -> Result<()> {
        let plan = self.analyze_playlist(url, tx).await?;
        let rows: Vec<Value> = plan
            .entries
            .iter()
            .map(|e| {
                let taken = e.sure || (also.contains(&e.pos) && !e.candidates.is_empty());
                let status = match (taken, e.have, e.sure) {
                    (true, true, _) => "have",
                    (true, false, true) => "new",
                    (true, false, false) => "take",
                    (false, ..) => "ask",
                };
                let best = e.candidates.first();
                json!({
                    "pick": e.pos,
                    "status": status,
                    "artist": best.map_or(e.source.artist.as_str(), |c| c.artist.as_str()),
                    "title": best.map_or(e.source.title.as_str(), |c| c.title.as_str()),
                    "album": best.map(|c| c.album.as_str()),
                    "note": if taken { String::new() } else { format!("{}; sent {:?}", e.reason, e.source.raw_title) },
                })
            })
            .collect();
        tx.send(Event::Data { data: json!(rows) }).await.context("client went away")?;
        let asks = rows.iter().filter(|r| r["status"] == "ask").count();
        if !apply {
            return info(
                tx,
                format!(
                    "{} to take, {asks} unsure; --apply imports, --also N,M takes unsure ones' top match too",
                    rows.len() - asks
                ),
            )
            .await;
        }
        let picks: Vec<(usize, usize)> = also.iter().map(|p| (*p, 0)).collect();
        let r = self.apply_playlist(&plan, &picks, tx).await?;
        ensure!(
            r.failed.is_empty(),
            "{} of {} downloads failed: {}; `td retry` queues them again",
            r.failed.len(),
            r.failed.len() + r.downloaded,
            r.failed.join(", ")
        );
        Ok(())
    }
}

/// yt-dlp's `--dump-json` for the link, boiled down.
fn source_from_video(url: &str, v: &Value) -> Source {
    let s = |k: &str| v[k].as_str().map(str::to_string);
    let raw_title = s("title").unwrap_or_default();
    let channel = s("channel").or_else(|| s("uploader"));
    let music = v["track"].is_string() && v["artist"].is_string()
        || channel.as_deref().is_some_and(|c| c.ends_with(" - Topic"))
        || url.contains("music.youtube.com");
    let (artist, title) = match (s("artist"), s("track")) {
        (Some(a), Some(t)) => (a.split(',').next().unwrap_or(&a).trim().to_string(), t),
        _ => split_artist_title(&clean_title(&raw_title), channel.as_deref()),
    };
    Source {
        url: Some(url.to_string()),
        video_id: s("id"),
        raw_title,
        channel,
        artist,
        title,
        duration_ms: v["duration"].as_f64().map(|d| (d * 1000.0) as i64),
        year: s("upload_date").and_then(|d| d.get(..4).and_then(|y| y.parse().ok())),
        music,
    }
}

/// Metadata for "exactly this video": tagged from the link, filed under its artist.
fn video_recording(src: &Source) -> Recording {
    let artist = if src.artist.is_empty() { "Unknown Artist".to_string() } else { src.artist.clone() };
    Recording {
        title: src.title.clone(),
        artist: artist.clone(),
        artist_mbid: None,
        album_artist: artist,
        album_artist_mbid: format!("youtube:{}", src.channel.clone().unwrap_or_default().to_lowercase()),
        album: "Requests".into(),
        album_mbid: format!("youtube:{}", src.video_id.clone().unwrap_or_default()),
        year: src.year,
        track_no: None,
        length_ms: src.duration_ms,
    }
}

/// Sure when the top candidate is the song as sent: same title, same artist, same version
/// words, uploaded by the artist (or a label/Topic channel), and for a music track about the
/// same length. Anything else is a question.
pub fn judge(src: &Source, candidates: &[Candidate]) -> (bool, String) {
    let Some(best) = candidates.first() else { return (false, "no MusicBrainz match".into()) };
    if norm(&best.title) != norm(&src.title) {
        return (false, format!("title differs: {:?} vs {:?}", src.title, best.title));
    }
    let (a, b) = (norm(&src.artist), norm(&best.artist));
    if !a.is_empty() && !(b.contains(&a) || a.contains(&b)) {
        return (false, format!("artist differs: {:?} vs {:?}", src.artist, best.artist));
    }
    let (sv, bv) = (version_words(&src.raw_title), version_words(&best.title));
    if sv != bv {
        return (false, format!("version differs: {:?} vs {:?}", src.raw_title, best.title));
    }
    if let Some(ch) = &src.channel {
        let c = norm(&channel_artist(ch));
        let lower = ch.to_lowercase();
        let official = lower.ends_with("vevo")
            || lower.ends_with(" - topic")
            || lower.contains("records")
            || lower.contains("music");
        if !official && !c.is_empty() && !(b.contains(&c) || c.contains(&b)) {
            return (false, format!("uploaded by {ch:?}, not {:?}", best.artist));
        }
    }
    if src.music
        && let (Some(x), Some(y)) = (src.duration_ms, best.length_ms)
        && (x - y).abs() > 20_000
    {
        return (false, format!("length differs: {}s vs {}s", x / 1000, y / 1000));
    }
    if candidates.len() > 1
        && norm(&candidates[1].title) == norm(&best.title)
        && candidates[1].length_ms.zip(best.length_ms).is_some_and(|(p, q)| (p - q).abs() > 20_000)
        && src.music
    {
        // Two takes with the same name: let the length decide only when we know it.
    }
    (true, "title, artist and version match".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playlist_links_and_flat_entries() {
        assert_eq!(playlist_id("https://www.youtube.com/playlist?list=PLmXxq").unwrap(), "PLmXxq");
        assert_eq!(playlist_id("https://music.youtube.com/watch?v=abc&list=OLAK5uy_x&si=1").unwrap(), "OLAK5uy_x");
        assert!(playlist_id("https://www.youtube.com/watch?v=abc&list=RDabc").unwrap_err().to_string().contains("mix"));
        assert!(playlist_id("https://music.youtube.com/playlist?list=LM").is_err());
        assert!(playlist_id("https://www.youtube.com/watch?v=abc").is_err());
        // What `--flat-playlist -J` gives per entry: no artist/track fields, so the title is split.
        let e = serde_json::json!({ "title": "Starship - Nothing's Gonna Stop Us Now (Official Music Video) [HD]",
            "duration": 268, "channel": "RHINO", "id": "3wxyN3z9PL4", "url": "https://www.youtube.com/watch?v=3wxyN3z9PL4" });
        let s = source_from_video("https://www.youtube.com/watch?v=3wxyN3z9PL4", &e);
        assert_eq!(
            (s.artist.as_str(), s.title.as_str(), s.duration_ms, s.music),
            ("Starship", "Nothing's Gonna Stop Us Now", Some(268_000), false)
        );
    }

    #[test]
    fn titles_are_cleaned_and_split() {
        assert_eq!(clean_title("Numb (Official Music Video) [4K UPGRADE] – Linkin Park"), "Numb – Linkin Park");
        assert_eq!(
            clean_title("Daft Punk - Get Lucky (Radio Edit) [Official Audio]"),
            "Daft Punk - Get Lucky (Radio Edit)"
        );
        assert_eq!(
            split_artist_title("Numb – Linkin Park", Some("Linkin Park")),
            ("Linkin Park".into(), "Numb".into())
        );
        assert_eq!(
            split_artist_title("Daft Punk - Get Lucky", Some("DaftPunkVEVO")),
            ("Daft Punk".into(), "Get Lucky".into())
        );
        assert_eq!(split_artist_title("Numb", Some("Linkin Park - Topic")), ("Linkin Park".into(), "Numb".into()));
        assert_eq!(channel_artist("ArianaGrandeVevo"), "ArianaGrande");
    }

    #[test]
    fn sure_only_when_everything_lines_up() {
        let cand = |title: &str, artist: &str, len: i64| Candidate {
            mbid: "m".into(),
            title: title.into(),
            artist: artist.into(),
            album: "A".into(),
            length_ms: Some(len),
        };
        let src = Source {
            raw_title: "Numb (Official Music Video) – Linkin Park".into(),
            channel: Some("Linkin Park".into()),
            artist: "Linkin Park".into(),
            title: "Numb".into(),
            duration_ms: Some(187_000),
            ..Default::default()
        };
        assert!(judge(&src, &[cand("Numb", "Linkin Park", 185_000)]).0);
        assert!(!judge(&src, &[]).0, "no match");
        assert!(!judge(&src, &[cand("Numb (live)", "Linkin Park", 185_000)]).0, "version");
        let cover = Source { channel: Some("Some Guy Covers".into()), ..src.clone() };
        assert!(!judge(&cover, &[cand("Numb", "Linkin Park", 185_000)]).0, "no-name cover");
        let remix = Source { raw_title: "Numb (Nightcore Remix) – Linkin Park".into(), ..src.clone() };
        assert!(!judge(&remix, &[cand("Numb", "Linkin Park", 185_000)]).0, "remix wanted, original found");
        let topic = Source {
            channel: Some("Linkin Park - Topic".into()),
            music: true,
            duration_ms: Some(240_000),
            ..src.clone()
        };
        assert!(!judge(&topic, &[cand("Numb", "Linkin Park", 185_000)]).0, "music track, wrong length");
    }
}
