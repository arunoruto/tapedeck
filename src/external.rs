//! Read-only libraries managed by something else (Lidarr's `releases/`). Their files are
//! indexed by recording id, and a recording that exists there is never downloaded, imported or
//! kept twice: the track points at the external file instead. Nothing under these paths is
//! ever written, moved, retagged or deleted.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use serde::Serialize;
use tokio::sync::mpsc::Sender;

use crate::{
    App, Event,
    app::{info, progress_at},
    db::{ExternalFile, Track},
    library,
    musicbrainz::strip_feat,
};

/// A file written in the last minute may still be arriving (Lidarr mid-import).
const SETTLE_SECS: i64 = 60;

#[derive(Serialize, Default, Debug)]
pub struct ScanReport {
    pub indexed: usize,
    pub added: usize,
    pub changed: usize,
    pub removed: usize,
    /// Tracks now pointing at an external file (newly covered, or moved with it).
    pub repointed: usize,
    /// Own copies an external file makes redundant, left for `td external dedupe`.
    pub redundant: usize,
    /// Tracks whose external file vanished with no replacement; they are wanted again.
    pub wanted_again: usize,
}

pub fn is_external(t: &Track) -> bool {
    t.status == "external"
}

/// Ready in the library, or present externally: either way the recording needs no download.
pub fn present(t: &Track) -> bool {
    t.status == "ready" || is_external(t)
}

/// The external copy of a track: by recording id, else by artist + title + length within 5 s.
/// Lossless first when there are several.
pub fn find_external<'a>(index: &'a [ExternalFile], t: &Track) -> Option<&'a ExternalFile> {
    let by_id = index.iter().find(|e| e.recording_mbid.as_deref() == Some(t.recording_mbid.as_str()));
    by_id.or_else(|| {
        let artist = norm(&t.artist);
        let title = norm(&t.title);
        let len = t.duration_ms?;
        index.iter().find(|e| {
            let a = norm(&e.artist);
            (a.starts_with(&artist) || artist.starts_with(&a))
                && norm(&e.title) == title
                && e.duration_ms.is_some_and(|ms| (ms - len).abs() <= 5_000)
        })
    })
}

/// Lowercase, feat. stripped, "&" as "and", every run of punctuation or dash collapsed to one
/// space: Lidarr's "When You're Gone" meets MusicBrainz's "When You’re Gone", "alt‐J" meets
/// "alt-J". Diacritics are kept as they are.
pub(crate) fn norm(s: &str) -> String {
    strip_feat(s)
        .to_lowercase()
        .replace('&', " and ")
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

impl App {
    /// Walk every external path, read tags for new or changed files, drop vanished ones, then
    /// reconcile the library: tracks follow moved files, lose vanished ones, and gain newly
    /// available ones. Playlists whose paths changed are pushed again.
    pub(crate) async fn external_scan(&self, tx: &Sender<Event>) -> Result<ScanReport> {
        let cfg = &self.config;
        let mut report = ScanReport::default();
        if cfg.external.paths.is_empty() {
            return Ok(report);
        }
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        let mut fresh: Vec<ExternalFile> = vec![];
        let mut gone: Vec<ExternalFile> = vec![];
        for root in &cfg.external.paths {
            let indexed: HashMap<PathBuf, ExternalFile> = self
                .db()
                .external_under(&root.to_string_lossy())?
                .into_iter()
                .map(|e| (PathBuf::from(&e.path), e))
                .collect();
            let mut seen: HashSet<PathBuf> = HashSet::new();
            let (mut added, mut changed) = (0, 0);
            let files = library::walk(root);
            let total = files.len();
            for (i, (path, size, mtime)) in files.into_iter().enumerate() {
                progress_at(tx, i + 1, total, format!("external: {}", path.display())).await?;
                if (i + 1) % 200 == 0 {
                    // The journal gets a heartbeat too; progress events stay out of it.
                    info(tx, format!("external: {}/{total} files looked at in {}", i + 1, root.display())).await?;
                }
                if now - mtime < SETTLE_SECS {
                    continue; // still being written; next scan
                }
                seen.insert(path.clone());
                // Unchanged files are skipped, except those indexed before album tags were kept.
                if indexed.get(&path).is_some_and(|e| e.size == size && e.mtime == mtime && e.album.is_some()) {
                    continue;
                }
                let found = library::inspect(&path);
                let e = ExternalFile {
                    path: path.to_string_lossy().into_owned(),
                    root: root.to_string_lossy().into_owned(),
                    size,
                    mtime,
                    recording_mbid: found.recording_mbid,
                    artist: found.artist,
                    title: found.title,
                    duration_ms: found.duration_ms,
                    album: found.album,
                    album_artist: found.album_artist,
                    track_no: found.track_no,
                    year: found.year,
                };
                self.db().upsert_external(&e)?;
                if indexed.contains_key(&path) {
                    changed += 1
                } else {
                    added += 1
                }
                fresh.push(e);
            }
            let mut removed = 0;
            for (path, e) in &indexed {
                if !seen.contains(path) && !path.exists() {
                    self.db().delete_external(&e.path)?;
                    gone.push(e.clone());
                    removed += 1;
                }
            }
            if added + changed + removed > 0 {
                info(
                    tx,
                    format!("external: +{added} added, {changed} changed, {removed} removed in {}", root.display()),
                )
                .await?;
            }
            report.added += added;
            report.changed += changed;
            report.removed += removed;
        }
        let index = self.db().external_all()?;
        report.indexed = index.len();
        let mut touched: Vec<i64> = vec![];

        // Files that vanished: follow the recording to another external copy, else want it again.
        for e in &gone {
            // Bind first: a guard inside the `for` expression would live across the awaits below.
            let at = self.db().tracks_at_path(&e.path)?;
            for t in at {
                match find_external(&index, &t) {
                    Some(other) => {
                        info(tx, format!("external moved: {} - {} → {}", t.artist, t.title, other.path)).await?;
                        touched.extend(self.repoint(&t, other, tx).await?);
                        report.repointed += 1;
                    }
                    None => {
                        self.db().reset_track(t.id)?;
                        info(tx, format!("external file gone: {} - {} is wanted again", t.artist, t.title)).await?;
                        report.wanted_again += 1;
                    }
                }
            }
        }

        // New or changed files: cover wanted and failed tracks right away; own copies depending
        // on `external.prefer_external`.
        if !fresh.is_empty() {
            let tracks = self.db().all_tracks()?;
            for t in tracks.iter().filter(|t| !is_external(t)) {
                let Some(e) = find_external(&fresh, t) else { continue };
                if t.status == "ready" {
                    match cfg.external.prefer_external.as_str() {
                        "auto" => {
                            info(tx, format!("external replaces own copy: {} - {} ← {}", t.artist, t.title, e.path))
                                .await?;
                            touched.extend(self.repoint(t, e, tx).await?);
                            report.repointed += 1;
                        }
                        "never" => {}
                        _ => {
                            info(
                                tx,
                                format!(
                                    "redundant own copy: {} - {}; external has {} (td external dedupe)",
                                    t.artist, t.title, e.path
                                ),
                            )
                            .await?;
                            report.redundant += 1;
                        }
                    }
                } else {
                    info(tx, format!("external covers {} - {}: {}", t.artist, t.title, e.path)).await?;
                    touched.extend(self.repoint(t, e, tx).await?);
                    report.repointed += 1;
                }
            }
        }

        touched.sort();
        touched.dedup();
        if !touched.is_empty() {
            let playlists = self.db().playlists()?;
            for p in playlists.iter().filter(|p| touched.contains(&p.id) && p.m3u_path.is_some()) {
                self.push(p, None, tx).await?;
            }
        }
        self.db().meta_set("external_scanned_at", &chrono_now())?;
        Ok(report)
    }

    /// Point `t` at the external file: the row becomes `external` with that absolute path, an
    /// own file goes (never the external one), m3u8s and backups follow. Returns the ids of the
    /// playlists that list it.
    pub(crate) async fn repoint(&self, t: &Track, e: &ExternalFile, tx: &Sender<Event>) -> Result<Vec<i64>> {
        let cfg = &self.config;
        let mut moves = HashMap::new();
        if let Some(old) = &t.path {
            let old_abs = cfg.library.join(old);
            if t.status == "ready" && self.db().tracks_with_path(old)?.len() == 1 {
                library::remove_track(&cfg.library, Path::new(old))?;
            }
            moves.insert(old_abs, PathBuf::from(&e.path));
        }
        self.db().mark_external(t.id, &e.path)?;
        let playlists = self.db().playlists_with_track(t.id)?;
        self.rewrite_m3us(Some(&playlists))?;
        self.follow_moves(&moves, tx).await?;
        Ok(playlists)
    }

    /// Own tracks that also exist externally: keep the external copy, drop ours.
    pub(crate) async fn external_dedupe(&self, dry_run: bool, tx: &Sender<Event>) -> Result<()> {
        let index = self.db().external_all()?;
        let tracks = self.db().all_tracks()?;
        let mut n = 0;
        let mut touched = vec![];
        for t in tracks.iter().filter(|t| t.status == "ready") {
            let Some(e) = find_external(&index, t) else { continue };
            n += 1;
            info(tx, format!("{} - {}: {} → {}", t.artist, t.title, t.path.as_deref().unwrap_or("?"), e.path)).await?;
            if !dry_run {
                touched.extend(self.repoint(t, e, tx).await?);
            }
        }
        if !dry_run && !touched.is_empty() {
            touched.sort();
            touched.dedup();
            let playlists = self.db().playlists()?;
            for p in playlists.iter().filter(|p| touched.contains(&p.id) && p.m3u_path.is_some()) {
                self.push(p, None, tx).await?;
            }
        }
        let verb = if dry_run { "would be replaced by" } else { "replaced by" };
        info(tx, format!("{n} own tracks {verb} external copies ({} external files indexed)", index.len())).await?;
        Ok(())
    }
}

/// `2026-10-18T01:02:03Z` without pulling in a date crate.
pub(crate) fn chrono_now() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, (rem % 3600) / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_by_id_then_by_name_and_length() {
        let ext = |path: &str, mbid: Option<&str>, artist: &str, title: &str, ms: i64| ExternalFile {
            path: path.into(),
            root: "/r".into(),
            size: 1,
            mtime: 1,
            recording_mbid: mbid.map(str::to_string),
            artist: artist.into(),
            title: title.into(),
            duration_ms: Some(ms),
            album: None,
            album_artist: None,
            track_no: None,
            year: None,
        };
        let index = vec![
            ext("/r/a.flac", Some("id-1"), "Linkin Park", "Numb", 187_000),
            ext("/r/b.mp3", None, "Linkin Park", "Faint", 162_000),
        ];
        let t = Track {
            recording_mbid: "id-1".into(),
            artist: "Linkin Park".into(),
            title: "Numb".into(),
            duration_ms: Some(400_000),
            ..Track::default()
        };
        assert_eq!(find_external(&index, &t).unwrap().path, "/r/a.flac", "id wins even when lengths differ");
        let t = Track {
            recording_mbid: "x".into(),
            artist: "LINKIN PARK feat. Someone".into(),
            title: "Faint!".into(),
            duration_ms: Some(165_000),
            ..Track::default()
        };
        assert_eq!(find_external(&index, &t).unwrap().path, "/r/b.mp3", "case and punctuation don't matter");
        assert_eq!(norm("When You’re Gone"), norm("When You're Gone"));
        assert_eq!(norm("Simon & Garfunkel"), norm("Simon and Garfunkel"));
        assert_eq!(norm("alt‐J"), "alt j");
        let t = Track {
            recording_mbid: "x".into(),
            artist: "Linkin Park".into(),
            title: "Faint".into(),
            duration_ms: Some(200_000),
            ..Track::default()
        };
        assert!(find_external(&index, &t).is_none(), "40 s off is a different cut");
        assert!(chrono_now().ends_with('Z') && chrono_now().len() == 20);
    }
}
