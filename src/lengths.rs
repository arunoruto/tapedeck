//! `td lengths`: own files that are not about as long as their MusicBrainz recording. Music-video
//! cuts that came in before the drift check, truncated downloads, radio edits. A file is off when
//! it misses the recording by more than `max_drift` (10 s, or 5 % of a long song), and each hit is
//! classified before the audio gets the blame:
//! - padded: leading and trailing silence explain the difference (same song, left alone),
//! - external: an external library has the recording (`td external dedupe` takes that one),
//! - metadata: another MusicBrainz recording of the song fits the file (`td retag` relinks it),
//! - release: a Deezer track of the same title and artist fits the file, so the audio is a real
//!   released version and MusicBrainz is the one that is off (Nightcall at 62 s); via the Deezer
//!   track's ISRC MusicBrainz often names the recording to relink to,
//! - broken (under half the length), longer, shorter: the audio is wrong.
//!
//! `--fix` downloads the last three afresh, ignoring the remembered URL, and replaces a file only
//! when the new one is closer and within max(5 s, 3 %). The old file and its sidecars move to
//! `<library>/.tapedeck-backup/<path>`, and lyrics are fetched again because the timings changed.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use serde_json::json;
use tokio::sync::mpsc::Sender;

use crate::{
    App, Event,
    app::{info, progress_at},
    db::Track,
    download::ytdlp::{file_secs, max_drift},
    external::{find_external, is_external, norm},
    library,
    lyrics::primary_artist,
    musicbrainz::{strip_feat, version_words},
};

/// Where `--fix` keeps the files it replaced, relative to the library.
const BACKUP: &str = ".tapedeck-backup";

/// Off by more than the drift limit: what kind of off. `silence` is (head, tail) in seconds.
fn classify(want: f64, got: f64, silence: Option<(f64, f64)>) -> Option<&'static str> {
    let tol = max_drift(want);
    if (got - want).abs() <= tol {
        return None;
    }
    if got < want * 0.5 {
        return Some("broken");
    }
    if let Some((head, tail)) = silence
        && (got - head - tail - want).abs() <= tol
    {
        return Some("padded");
    }
    Some(if got > want { "longer" } else { "shorter" })
}

/// Deezer search hits with the same title (no extra "live"/"radio edit"), the same primary artist
/// and a length within `max_drift(got)` of the file: (seconds, ISRC).
fn deezer_fits(v: &Value, artist: &str, title: &str, got: f64) -> Vec<(f64, Option<String>)> {
    let (artist, title) = (norm(&primary_artist(artist)), norm(&strip_feat(title)));
    let ours = version_words(&title);
    v["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|h| {
            let t = h["title"].as_str().unwrap_or("");
            norm(&strip_feat(h["title_short"].as_str().unwrap_or(t))) == title
                && version_words(t).iter().all(|w| ours.contains(w))
                && norm(h["artist"]["name"].as_str().unwrap_or("")) == artist
        })
        .filter_map(|h| Some((h["duration"].as_f64()?, h["isrc"].as_str().map(str::to_owned))))
        .filter(|(d, _)| (d - got).abs() <= max_drift(got))
        .collect()
}

/// Leading and trailing silence from ffmpeg's silencedetect log, for a file `len` seconds long.
fn edge_silence(log: &str, len: f64) -> (f64, f64) {
    let after =
        |line: &str, key: &str| -> Option<f64> { line.split_once(key)?.1.split_whitespace().next()?.parse().ok() };
    let mut spans: Vec<(f64, Option<f64>)> = vec![];
    for line in log.lines() {
        if let Some(s) = after(line, "silence_start: ") {
            spans.push((s, None));
        } else if let (Some(e), Some(last)) = (after(line, "silence_end: "), spans.last_mut()) {
            last.1 = Some(e);
        }
    }
    let head = spans.first().filter(|s| s.0 <= 0.5).and_then(|s| s.1).unwrap_or(0.0);
    let tail = spans
        .last()
        .filter(|s| (spans.len() > 1 || head == 0.0) && s.1.is_none_or(|e| e >= len - 0.5))
        .map_or(0.0, |s| len - s.0);
    (head, tail)
}

impl App {
    pub(crate) async fn lengths(&self, only: Option<i64>, fix: bool, tx: &Sender<Event>) -> Result<()> {
        let cfg = &self.config;
        let tracks: Vec<Track> = self
            .db()
            .all_tracks()?
            .into_iter()
            .filter(|t| t.status == "ready" && !is_external(t) && t.duration_ms.is_some() && t.path.is_some())
            .filter(|t| only.is_none_or(|id| t.id == id))
            .collect();
        let external = self.db().external_all()?;
        let ffmpeg = match cfg.download.ffmpeg_bin.as_deref().map(Path::new) {
            Some(p) if p.is_dir() => p.join("ffmpeg"),
            Some(p) => p.to_path_buf(),
            None => PathBuf::from("ffmpeg"),
        };
        let mut silence_ok = true;
        // (track, MusicBrainz seconds, file seconds, kind, note)
        let mut hits: Vec<(&Track, f64, f64, &str, Option<String>)> = vec![];
        for (i, t) in tracks.iter().enumerate() {
            progress_at(tx, i + 1, tracks.len(), format!("{} - {}", t.artist, t.title)).await?;
            let (Some(ms), Some(rel)) = (t.duration_ms, &t.path) else { continue };
            let (want, path) = (ms as f64 / 1000.0, cfg.library.join(rel));
            let Some(got) = file_secs(&path) else {
                hits.push((t, want, 0.0, "unreadable", None));
                continue;
            };
            let Some(mut kind) = classify(want, got, None) else { continue };
            if kind == "longer" && silence_ok {
                let out = tokio::process::Command::new(&ffmpeg)
                    .args(["-hide_banner", "-nostats", "-i"])
                    .arg(&path)
                    .args(["-af", "silencedetect=noise=-50dB:d=0.5", "-f", "null", "-"])
                    .output()
                    .await;
                match out {
                    Ok(o) if o.status.success() => {
                        let s = edge_silence(&String::from_utf8_lossy(&o.stderr), got);
                        kind = classify(want, got, Some(s)).unwrap_or(kind);
                    }
                    Ok(o) => {
                        tracing::debug!("silencedetect {}: {}", path.display(), String::from_utf8_lossy(&o.stderr))
                    }
                    Err(e) => {
                        silence_ok = false;
                        info(tx, format!("no silence check, running {}: {e}", ffmpeg.display())).await?;
                    }
                }
            }
            let mut note = None;
            if matches!(kind, "longer" | "shorter") {
                if let Some(e) = find_external(&external, t) {
                    kind = "external";
                    note = Some(e.path.clone());
                } else {
                    match self.mb.search_candidates(&t.artist, &t.title, Some((got * 1000.0) as i64)).await {
                        Ok(cands) => {
                            let fits =
                                |l: Option<i64>| l.is_some_and(|l| (l as f64 / 1000.0 - got).abs() <= max_drift(got));
                            if let Some(c) = cands.iter().find(|c| {
                                c.mbid != t.recording_mbid && norm(&c.title) == norm(&t.title) && fits(c.length_ms)
                            }) {
                                kind = "metadata";
                                let secs = c.length_ms.unwrap_or(0) / 1000;
                                note =
                                    Some(format!("{secs}s on {:?}: td retag {} --recording {}", c.album, t.id, c.mbid));
                            }
                        }
                        Err(e) => info(tx, format!("musicbrainz {} - {}: {e:#}", t.artist, t.title)).await?,
                    }
                }
            }
            if matches!(kind, "longer" | "shorter") {
                match self.deezer_release(t, got).await {
                    Ok(Some(n)) => {
                        kind = "release";
                        note = Some(n);
                    }
                    Ok(None) => {}
                    Err(e) => info(tx, format!("deezer {} - {}: {e:#}", t.artist, t.title)).await?,
                }
            }
            hits.push((t, want, got, kind, note));
        }

        let mmss = |s: f64| format!("{}:{:02}", s as i64 / 60, s as i64 % 60);
        let rows: Vec<_> = hits
            .iter()
            .map(|(t, want, got, kind, note)| {
                json!({
                    "id": t.id, "kind": kind, "artist": t.artist, "title": t.title,
                    "length": format!("{} → {} ({:+.0}s)", mmss(*want), mmss(*got), got - want),
                    "note": note, "path": t.path,
                })
            })
            .collect();
        let mut counts: Vec<(&str, usize)> = vec![];
        for (_, _, _, kind, _) in &hits {
            match counts.iter_mut().find(|(k, _)| k == kind) {
                Some(c) => c.1 += 1,
                None => counts.push((kind, 1)),
            }
        }
        let summary = counts.iter().map(|(k, n)| format!("{n} {k}")).collect::<Vec<_>>().join(", ");
        tx.send(Event::Data { data: json!(rows) }).await.context("client went away")?;
        info(
            tx,
            format!(
                "{} files checked, {} off: {}",
                tracks.len(),
                hits.len(),
                if summary.is_empty() { "none".into() } else { summary }
            ),
        )
        .await?;

        let todo: Vec<_> = hits.iter().filter(|h| matches!(h.3, "broken" | "longer" | "shorter")).collect();
        if !fix {
            if !todo.is_empty() {
                info(
                    tx,
                    format!(
                        "--fix downloads the {} broken/longer/shorter ones again, old files to {BACKUP}/",
                        todo.len()
                    ),
                )
                .await?;
            }
            return Ok(());
        }
        let (mut fixed, mut kept) = (0, 0);
        let mut moves: HashMap<PathBuf, PathBuf> = HashMap::new();
        let (mut keys, mut files) = (vec![], vec![]);
        for (i, (t, want, got, _, _)) in todo.iter().enumerate() {
            progress_at(tx, i + 1, todo.len(), format!("replacing {} - {}", t.artist, t.title)).await?;
            match self.replace_audio(t, *want, (got - want).abs()).await {
                Ok(rel) => {
                    let old = t.path.as_deref().unwrap_or_default();
                    info(tx, format!("replaced {} - {}; old file in {BACKUP}/{old}", t.artist, t.title)).await?;
                    if rel != Path::new(old) {
                        moves.insert(cfg.library.join(old), cfg.library.join(&rel));
                    }
                    let key = format!("t:{}", t.id);
                    self.db().lyrics_delete(&key)?;
                    keys.push(key);
                    files.push(cfg.library.join(&rel));
                    fixed += 1;
                }
                Err(e) => {
                    info(tx, format!("kept {} - {}: {e:#}", t.artist, t.title)).await?;
                    kept += 1;
                }
            }
        }
        if !moves.is_empty() {
            self.rewrite_m3us(None)?;
            self.follow_moves(&moves, tx).await?;
        }
        if cfg.lyrics.enable
            && !keys.is_empty()
            && let Err(e) = self.lyrics_fetch(Some(&keys), crate::lyrics::Selection::default(), tx).await
        {
            info(tx, format!("lyrics: {e:#}")).await?;
        }
        for target in &self.targets {
            if !files.is_empty()
                && let Err(e) = target.refresh_lyrics(&files).await
            {
                info(tx, format!("{}: refresh: {e:#}", target.name())).await?;
            }
        }
        info(tx, format!("{fixed} replaced, {kept} kept")).await
    }

    /// A Deezer release of `t` as long as the file (`got` seconds), as a note: its length and,
    /// when its ISRC leads to a fitting MusicBrainz recording, the `td retag` line.
    async fn deezer_release(&self, t: &Track, got: f64) -> Result<Option<String>> {
        let http = crate::http();
        // The plain query; the advanced `artist:"…" track:"…"` form often finds nothing.
        let q = format!("{} {}", primary_artist(&t.artist), strip_feat(&t.title));
        let v: Value = http
            .get(format!("https://api.deezer.com/search?limit=25&q={}", urlencoding::encode(&q)))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let Some((secs, isrc)) = deezer_fits(&v, &t.artist, &t.title, got).into_iter().next() else { return Ok(None) };
        let mut note = format!("Deezer has it at {}s", secs as i64);
        if let Some(isrc) = isrc.as_deref() {
            let fits = |l: &Option<i64>| l.is_some_and(|l| (l as f64 / 1000.0 - got).abs() <= max_drift(got));
            match self.mb.isrc(isrc).await {
                Ok(recs) => match recs.iter().find(|(mbid, l)| *mbid != t.recording_mbid && fits(l)) {
                    Some((mbid, _)) => note += &format!(": td retag {} --recording {mbid}", t.id),
                    None => note += &format!(", ISRC {isrc}"),
                },
                Err(_) => note += &format!(", ISRC {isrc}"),
            }
        }
        Ok(Some(note))
    }

    /// Download `t` afresh and swap it in when the new file is closer to `want` seconds than the
    /// old one (`old_off` seconds off) and within max(5 s, 3 %). Returns the new relative path.
    async fn replace_audio(&self, t: &Track, want: f64, old_off: f64) -> Result<PathBuf> {
        let cfg = &self.config;
        let fresh = Track { source_url: None, ..t.clone() };
        let got = self.downloader.download(&fresh, &cfg.tmp_dir()).await?;
        let off = file_secs(&got.path).map_or(f64::MAX, |s| (s - want).abs());
        if off > (want * 0.03).max(5.0) || off >= old_off {
            let _ = std::fs::remove_file(&got.path);
            bail!("the best new download is {off:.0}s off, not close enough");
        }
        let old = PathBuf::from(t.path.as_deref().context("track has no file")?);
        let backup = library::unique(&cfg.library, &Path::new(BACKUP).join(&old));
        library::move_track(&cfg.library, &old, &backup)?;
        match library::file_track(&cfg.library, &cfg.path_template, t, &got.path, false) {
            Ok(rel) => {
                self.db().mark_ready(t.id, &rel.to_string_lossy(), got.source_url.as_deref())?;
                Ok(rel)
            }
            Err(e) => {
                let _ = library::move_track(&cfg.library, &backup, &old);
                let _ = std::fs::remove_file(&got.path);
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_by_tolerance_silence_and_ratio() {
        assert_eq!(classify(200.0, 209.0, None), None, "within 10 s");
        assert_eq!(classify(467.0, 445.0, None), None, "within 5 % of a long song");
        assert_eq!(classify(252.0, 349.0, None), Some("longer"));
        assert_eq!(classify(467.0, 256.0, None), Some("shorter"));
        assert_eq!(classify(157.0, 9.0, None), Some("broken"));
        assert_eq!(classify(200.0, 215.0, Some((3.0, 4.0))), Some("padded"));
        assert_eq!(classify(252.0, 349.0, Some((1.0, 2.0))), Some("longer"));
    }

    #[test]
    fn deezer_hit_must_match_title_artist_and_length() {
        let v = serde_json::json!({ "data": [
            { "id": 1, "title": "Nightcall (Radio Edit)", "title_short": "Nightcall", "duration": 200, "artist": { "name": "Kavinsky" } },
            { "id": 2, "title": "Nightcall", "title_short": "Nightcall", "duration": 259, "isrc": "FRS710900410", "artist": { "name": "Kavinsky" } },
            { "id": 3, "title": "Nightcall", "title_short": "Nightcall", "duration": 258, "artist": { "name": "Karaoke Stars" } },
            { "id": 4, "title": "Nightcall (Live)", "title_short": "Nightcall", "duration": 257, "artist": { "name": "Kavinsky" } },
        ]});
        assert_eq!(deezer_fits(&v, "Kavinsky", "Nightcall", 257.0), vec![(259.0, Some("FRS710900410".to_string()))]);
        assert!(deezer_fits(&v, "Kavinsky", "Nightcall", 330.0).is_empty());
    }

    #[test]
    fn reads_edge_silence() {
        let log = "[silencedetect @ 0x1] silence_start: 0\n\
                   [silencedetect @ 0x1] silence_end: 3.02 | silence_duration: 3.02\n\
                   [silencedetect @ 0x1] silence_start: 101.5\n\
                   [silencedetect @ 0x1] silence_end: 102.1 | silence_duration: 0.6\n\
                   [silencedetect @ 0x1] silence_start: 196.5\n";
        assert_eq!(edge_silence(log, 200.0), (3.02, 3.5));
        assert_eq!(edge_silence("", 200.0), (0.0, 0.0));
        let all_silent = "silence_start: 0\nsilence_end: 200 | silence_duration: 200\n";
        assert_eq!(edge_silence(all_silent, 200.0), (200.0, 0.0), "one span counts once");
    }
}
