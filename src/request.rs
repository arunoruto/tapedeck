//! Song requests: a link or "Artist - Title" comes in (Telegram, `td request`), tapedeck works
//! out which recording it is, fetches the best audio for it, files it, and puts it on this
//! year's "Requests" playlist. It asks only when it is not sure: a cover by someone else, a
//! remix the recording doesn't carry, no MusicBrainz match at all.

use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc::Sender;

use crate::{
    App, Event,
    app::{display_name, info, new_track},
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
        let playlist = self.add_to_requests(id, tx).await?;
        Ok(Outcome { track, playlist, already_had })
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
