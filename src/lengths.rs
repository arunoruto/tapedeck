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
//! The audit is stored (`lengths` table) so someone can listen and judge: `--mark good|bad|relink`
//! or the TUI's Review tab. A verdict holds while the file stays the same; good ones drop out of
//! the listing. `--fix` acts on verdicts only: "relink" retags to the suggested recording, "bad"
//! is downloaded afresh (ignoring the remembered URL) and replaced only when the new file is
//! closer and within max(5 s, 3 %). The old file and its sidecars move to
//! `<library>/.tapedeck-backup/<path>`, and lyrics are fetched again because the timings changed.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use serde_json::json;
use tokio::sync::mpsc::Sender;

use crate::{
    App, Event,
    app::{info, progress_at},
    cli::LengthsArgs,
    db::{LengthRow, Track},
    download::ytdlp::{file_secs, max_drift},
    external::{find_external, is_external, norm},
    library,
    lyrics::primary_artist,
    musicbrainz::{strip_feat, version_words},
};

/// Where `--fix` keeps the files it replaced, relative to the library.
const BACKUP: &str = ".tapedeck-backup";

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn bump(counts: &mut Vec<(String, usize)>, kind: &str) {
    match counts.iter_mut().find(|(k, _)| k == kind) {
        Some(c) => c.1 += 1,
        None => counts.push((kind.to_string(), 1)),
    }
}

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
    /// `td lengths`: mark one row, fix the flagged ones, audit, then list what is stored.
    pub(crate) async fn lengths(&self, cmd: LengthsArgs, tx: &Sender<Event>) -> Result<()> {
        let LengthsArgs { track, fix, mark, list, verdict } = cmd;
        if let Some(v) = mark {
            let id = track.context("--mark needs a track id")?;
            let row = self.db().length_get(id)?.with_context(|| format!("track {id} is not in the audit"))?;
            let v = match v.as_str() {
                "none" | "u" | "undecided" => None,
                "good" | "bad" => Some(v),
                "relink" => {
                    ensure!(row.relink.is_some(), "no recording to relink to; use td retag {id} --recording …");
                    Some(v)
                }
                other => bail!("verdict {other:?}: one of good, bad, relink, none"),
            };
            self.db().length_mark(id, v.as_deref())?;
            return info(tx, format!("track {id}: {}", v.as_deref().unwrap_or("undecided"))).await;
        }
        if fix {
            return self.lengths_fix(track, tx).await;
        }
        if !list {
            self.lengths_scan(track, tx).await?;
        }
        let shown: Vec<Value> = self
            .lengths_rows()?
            .into_iter()
            .filter(|r| track.is_none_or(|id| r["id"] == id))
            .filter(|r| match verdict.as_deref() {
                // Files judged good stay out of the way unless asked for.
                None => r["verdict"] != "good",
                Some("all") => true,
                Some("none") => r["verdict"].is_null(),
                Some(v) => r["verdict"] == v,
            })
            .collect();
        tx.send(Event::Data { data: json!(shown) }).await.context("client went away")
    }

    /// The stored audit joined with the tracks, worst first, as the CLI and the TUI show it.
    fn lengths_rows(&self) -> Result<Vec<Value>> {
        let rows = self.db().lengths_all()?;
        let tracks: HashMap<i64, Track> = self.db().all_tracks()?.into_iter().map(|t| (t.id, t)).collect();
        let mmss = |ms: i64| format!("{}:{:02}", ms / 60000, (ms / 1000) % 60);
        let mut out: Vec<(i64, Value)> = rows
            .into_iter()
            .filter_map(|r| {
                let t = tracks.get(&r.track_id)?;
                let delta = r.file_ms - r.mb_ms;
                let v = json!({
                    "id": r.track_id, "verdict": r.verdict, "kind": r.kind, "artist": t.artist, "title": t.title,
                    "length": format!("{} → {} ({:+}s)", mmss(r.mb_ms), mmss(r.file_ms), delta / 1000),
                    "note": r.note, "path": t.path, "mb_ms": r.mb_ms, "file_ms": r.file_ms, "relink": r.relink,
                });
                Some((delta.abs(), v))
            })
            .collect();
        out.sort_by_key(|(d, _)| std::cmp::Reverse(*d));
        Ok(out.into_iter().map(|(_, v)| v).collect())
    }

    /// Check every own file (or `only`) and store what is off. A row with a verdict is kept as it
    /// is while the file stays the same size and age; a file that is fine again leaves the audit.
    async fn lengths_scan(&self, only: Option<i64>, tx: &Sender<Event>) -> Result<()> {
        let cfg = &self.config;
        let tracks: Vec<Track> = self
            .db()
            .all_tracks()?
            .into_iter()
            .filter(|t| t.status == "ready" && !is_external(t) && t.duration_ms.is_some() && t.path.is_some())
            .filter(|t| only.is_none_or(|id| t.id == id))
            .collect();
        let stored: HashMap<i64, LengthRow> = self.db().lengths_all()?.into_iter().map(|r| (r.track_id, r)).collect();
        let external = self.db().external_all()?;
        let ffmpeg = match cfg.download.ffmpeg_bin.as_deref().map(Path::new) {
            Some(p) if p.is_dir() => p.join("ffmpeg"),
            Some(p) => p.to_path_buf(),
            None => PathBuf::from("ffmpeg"),
        };
        let mut silence_ok = true;
        let mut counts: Vec<(String, usize)> = vec![];
        for (i, t) in tracks.iter().enumerate() {
            progress_at(tx, i + 1, tracks.len(), format!("{} - {}", t.artist, t.title)).await?;
            let (Some(ms), Some(rel)) = (t.duration_ms, &t.path) else { continue };
            let (want, path) = (ms as f64 / 1000.0, cfg.library.join(rel));
            let (size, mtime) = std::fs::metadata(&path)
                .map(|m| {
                    let mtime = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
                    (Some(m.len() as i64), mtime.map(|d| d.as_secs() as i64))
                })
                .unwrap_or((None, None));
            if let Some(r) = stored.get(&t.id)
                && r.verdict.is_some()
                && r.size == size
                && r.mtime == mtime
            {
                bump(&mut counts, &r.kind);
                continue;
            }
            let got = file_secs(&path);
            let found = match got {
                None => Some(("unreadable", None, None)),
                Some(got) => match classify(want, got, None) {
                    None => None,
                    Some(kind) => {
                        Some(self.explain(t, &path, want, got, kind, &external, &ffmpeg, &mut silence_ok, tx).await?)
                    }
                },
            };
            let Some((kind, note, relink)) = found else {
                self.db().length_delete(t.id)?;
                continue;
            };
            bump(&mut counts, kind);
            self.db().length_set(&LengthRow {
                track_id: t.id,
                kind: kind.to_string(),
                mb_ms: ms,
                file_ms: got.map_or(0, |s| (s * 1000.0) as i64),
                note,
                relink,
                size,
                mtime,
                verdict: None,
                decided_at: None,
                checked_at: now(),
            })?;
        }
        let total: usize = counts.iter().map(|c| c.1).sum();
        let summary = counts.iter().map(|(k, n)| format!("{n} {k}")).collect::<Vec<_>>().join(", ");
        let summary = if summary.is_empty() { "none".to_string() } else { summary };
        info(tx, format!("{} files checked, {total} off: {summary}", tracks.len())).await?;
        info(tx, "flag them in the TUI's Review tab or with td lengths ID --mark good|bad|relink; --fix acts on bad and relink".into()).await
    }

    /// Off by more than the drift limit: rule out padding, an external copy, a better
    /// recording and a real release before the audio gets the blame. (kind, note, relink MBID)
    #[allow(clippy::too_many_arguments)]
    async fn explain(
        &self,
        t: &Track,
        path: &Path,
        want: f64,
        got: f64,
        mut kind: &'static str,
        external: &[crate::db::ExternalFile],
        ffmpeg: &Path,
        silence_ok: &mut bool,
        tx: &Sender<Event>,
    ) -> Result<(&'static str, Option<String>, Option<String>)> {
        if kind == "longer" && *silence_ok {
            let out = tokio::process::Command::new(ffmpeg)
                .args(["-hide_banner", "-nostats", "-i"])
                .arg(path)
                .args(["-af", "silencedetect=noise=-50dB:d=0.5", "-f", "null", "-"])
                .output()
                .await;
            match out {
                Ok(o) if o.status.success() => {
                    let s = edge_silence(&String::from_utf8_lossy(&o.stderr), got);
                    kind = classify(want, got, Some(s)).unwrap_or(kind);
                }
                Ok(o) => tracing::debug!("silencedetect {}: {}", path.display(), String::from_utf8_lossy(&o.stderr)),
                Err(e) => {
                    *silence_ok = false;
                    info(tx, format!("no silence check, running {}: {e}", ffmpeg.display())).await?;
                }
            }
        }
        if !matches!(kind, "longer" | "shorter") {
            return Ok((kind, None, None));
        }
        if let Some(e) = find_external(external, t) {
            return Ok(("external", Some(e.path.clone()), None));
        }
        let fits = |l: Option<i64>| l.is_some_and(|l| (l as f64 / 1000.0 - got).abs() <= max_drift(got));
        match self.mb.search_candidates(&t.artist, &t.title, Some((got * 1000.0) as i64)).await {
            Ok(cands) => {
                if let Some(c) = cands
                    .iter()
                    .find(|c| c.mbid != t.recording_mbid && norm(&c.title) == norm(&t.title) && fits(c.length_ms))
                {
                    let note = format!("{}s on {:?}", c.length_ms.unwrap_or(0) / 1000, c.album);
                    return Ok(("metadata", Some(note), Some(c.mbid.clone())));
                }
            }
            Err(e) => info(tx, format!("musicbrainz {} - {}: {e:#}", t.artist, t.title)).await?,
        }
        match self.deezer_release(t, got).await {
            Ok(Some((note, relink))) => Ok(("release", Some(note), relink)),
            Ok(None) => Ok((kind, None, None)),
            Err(e) => {
                info(tx, format!("deezer {} - {}: {e:#}", t.artist, t.title)).await?;
                Ok((kind, None, None))
            }
        }
    }

    /// Act on the verdicts: "bad" files are downloaded again, "relink" ones retagged to the
    /// recording that fits them. Rows without a verdict are never touched.
    async fn lengths_fix(&self, only: Option<i64>, tx: &Sender<Event>) -> Result<()> {
        let cfg = &self.config;
        let todo: Vec<LengthRow> = self
            .db()
            .lengths_all()?
            .into_iter()
            .filter(|r| only.is_none_or(|id| r.track_id == id))
            .filter(|r| matches!(r.verdict.as_deref(), Some("bad" | "relink")))
            .collect();
        if todo.is_empty() {
            return info(tx, "nothing flagged bad or relink; flag with --mark or in the TUI's Review tab".into()).await;
        }
        let (mut fixed, mut kept) = (0, 0);
        let mut moves: HashMap<PathBuf, PathBuf> = HashMap::new();
        let (mut keys, mut files) = (vec![], vec![]);
        for (i, r) in todo.iter().enumerate() {
            let Some(t) = self.db().track_by_id(r.track_id)? else { continue };
            progress_at(tx, i + 1, todo.len(), format!("fixing {} - {}", t.artist, t.title)).await?;
            if r.verdict.as_deref() == Some("relink") {
                let mbid = r.relink.clone().unwrap_or_default();
                match self.retag(t.id, mbid, tx).await {
                    Ok(()) => {
                        self.db().length_delete(t.id)?;
                        fixed += 1;
                    }
                    Err(e) => {
                        info(tx, format!("kept {} - {}: {e:#}", t.artist, t.title)).await?;
                        kept += 1;
                    }
                }
                continue;
            }
            let want = r.mb_ms as f64 / 1000.0;
            match self.replace_audio(&t, want, (r.file_ms - r.mb_ms).abs() as f64 / 1000.0).await {
                Ok(rel) => {
                    let old = t.path.as_deref().unwrap_or_default();
                    info(tx, format!("replaced {} - {}; old file in {BACKUP}/{old}", t.artist, t.title)).await?;
                    if rel != Path::new(old) {
                        moves.insert(cfg.library.join(old), cfg.library.join(&rel));
                    }
                    let key = format!("t:{}", t.id);
                    self.db().lyrics_delete(&key)?;
                    self.db().length_delete(t.id)?;
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
        info(tx, format!("{fixed} fixed, {kept} kept")).await
    }

    /// A Deezer release of `t` as long as the file (`got` seconds): a note with its length, and
    /// the MusicBrainz recording its ISRC leads to when that one fits the file.
    async fn deezer_release(&self, t: &Track, got: f64) -> Result<Option<(String, Option<String>)>> {
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
        let mut relink = None;
        if let Some(isrc) = isrc.as_deref() {
            let fits = |l: &Option<i64>| l.is_some_and(|l| (l as f64 / 1000.0 - got).abs() <= max_drift(got));
            match self.mb.isrc(isrc).await.map(|recs| recs.into_iter().find(|(m, l)| *m != t.recording_mbid && fits(l)))
            {
                Ok(Some((mbid, _))) => relink = Some(mbid),
                _ => note += &format!(", ISRC {isrc}"),
            }
        }
        Ok(Some((note, relink)))
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
    fn lengths_command_survives_the_socket() {
        let cmd = crate::cli::Command::Lengths(LengthsArgs {
            track: Some(7),
            mark: Some("bad".into()),
            ..Default::default()
        });
        let back: crate::cli::Command = serde_json::from_str(&serde_json::to_string(&cmd).unwrap()).unwrap();
        assert!(
            matches!(back, crate::cli::Command::Lengths(LengthsArgs { track: Some(7), mark: Some(m), .. }) if m == "bad")
        );
        let bare: crate::cli::Command = serde_json::from_str(r#"{"cmd":"lengths"}"#).unwrap();
        assert!(matches!(bare, crate::cli::Command::Lengths(LengthsArgs { track: None, fix: false, .. })));
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
