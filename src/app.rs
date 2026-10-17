//! The one place commands are executed, whether invoked locally or through the daemon.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Mutex,
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc::Sender;

use crate::{
    cli::{Command, RemoteCmd},
    config::Config,
    db::{Db, NewTrack, Playlist, Track},
    download::{self, Downloader},
    library, listenbrainz, m3u,
    musicbrainz::{MusicBrainz, Recording},
    sync::{self, RemotePlaylist, SyncTarget, glob_match},
};

/// Streamed back to whoever invoked a command. `--json` prints these verbatim.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Info {
        msg: String,
    },
    /// A counter that replaces the previous one on a terminal ("resolving 812/1582 …").
    Progress {
        msg: String,
    },
    Data {
        data: Value,
    },
    Error {
        msg: String,
    },
}

pub struct App {
    pub config: Config,
    db: Mutex<Db>,
    lb: listenbrainz::ListenBrainz,
    mb: MusicBrainz,
    downloader: Box<dyn Downloader>,
    targets: Vec<Box<dyn SyncTarget>>,
    // ponytail: one command at a time; downloads and the sync tick never overlap. Per-command locking if this ever hurts.
    busy: tokio::sync::Mutex<()>,
}

impl App {
    pub fn open(config: Config) -> Result<Self> {
        let http = reqwest::Client::builder().user_agent(crate::USER_AGENT).build()?;
        Ok(Self {
            db: Mutex::new(Db::open(&config.database_path())?),
            lb: listenbrainz::ListenBrainz::new(http.clone(), config.listenbrainz.token.clone()),
            mb: MusicBrainz::new(http),
            downloader: download::from_config(&config)?,
            targets: sync::from_config(&config)?,
            busy: tokio::sync::Mutex::new(()),
            config,
        })
    }

    /// Run a command, streaming events. Never returns an error: failures become `Event::Error`.
    pub async fn run(&self, cmd: Command, tx: Sender<Event>) {
        let _guard = self.busy.lock().await;
        if let Err(e) = self.exec(cmd, &tx).await {
            let _ = tx.send(Event::Error { msg: format!("{e:#}") }).await;
        }
    }

    async fn exec(&self, cmd: Command, tx: &Sender<Event>) -> Result<()> {
        match cmd {
            Command::Daemon | Command::Tui => {
                bail!("this command runs in the foreground; it cannot be sent to a running daemon")
            }
            Command::Sync { playlist, no_push, no_fetch } => self.sync(playlist, no_push, no_fetch, tx).await,
            Command::Status => {
                let counts: serde_json::Map<_, _> =
                    self.db().status_counts()?.into_iter().map(|(k, v)| (k, json!(v))).collect();
                let playlists = self.db().playlists()?.len();
                let shared = self.db().shared_paths()?;
                for (path, n) in &shared {
                    info(tx, format!("warning: {n} tracks share one file: {path}")).await?;
                }
                data(tx, json!({ "tracks": counts, "playlists": playlists, "shared_paths": shared.len() })).await
            }
            // Bind before awaiting: the DB guard must not live across an await point.
            Command::Search { query } => {
                let tracks = self.db().search(&query)?;
                data(tx, json!(tracks)).await
            }
            Command::Playlists => {
                let playlists = self.db().playlists()?;
                data(tx, json!(playlists)).await
            }
            Command::Push { playlist, to, title } => {
                let path = Path::new(&playlist);
                if path.extension().is_some_and(|e| e == "m3u8") && path.is_file() {
                    return self.push_file(path, title, to.as_deref(), tx).await;
                }
                let p = self.db().find_playlist(&playlist)?.with_context(|| format!("no playlist {playlist:?}"))?;
                self.push(&p, to.as_deref(), tx).await
            }
            Command::Unpush { playlist, to, yes } => {
                let path = Path::new(&playlist);
                let title = if path.extension().is_some_and(|e| e == "m3u8") && path.is_file() {
                    self.file_title(path)?
                } else {
                    playlist.clone()
                };
                for target in self.targets(to.as_deref())? {
                    let hits: Vec<_> =
                        target.list_playlists().await?.into_iter().filter(|p| p.title == title && !p.smart).collect();
                    if hits.is_empty() {
                        info(tx, format!("{}: nothing called {title:?}", target.name())).await?;
                        continue;
                    }
                    if !yes {
                        info(
                            tx,
                            format!(
                                "{}: would remove {title:?} ({} tracks); pass --yes",
                                target.name(),
                                hits[0].tracks
                            ),
                        )
                        .await?;
                        continue;
                    }
                    self.backup(target, &hits, &self.config.playlist_dir().join("backup"), tx).await?;
                    for p in &hits {
                        target.delete_playlist(&p.id).await?;
                        info(
                            tx,
                            format!(
                                "{}: removed {} ({} tracks); the local file is untouched",
                                target.name(),
                                p.title,
                                p.tracks
                            ),
                        )
                        .await?;
                    }
                    let playlists = self.db().playlists()?;
                    for p in playlists.iter().filter(|p| self.remote_title(p, &playlists) == title) {
                        self.db().clear_sync(p.id, target.name())?;
                    }
                }
                Ok(())
            }
            Command::Retry { track: None } => {
                let n = self.db().retry_failed()?;
                info(tx, format!("{n} tracks reset to wanted")).await
            }
            Command::Retry { track: Some(id) } => {
                let t = self.db().track_by_id(id)?.with_context(|| format!("no track with id {id}"))?;
                if let Some(rel) = &t.path {
                    library::remove_track(&self.config.library, Path::new(rel))?;
                }
                self.db().reset_track(id)?;
                info(tx, format!("{} - {} reset to wanted; `td sync --no-fetch` downloads it", t.artist, t.title)).await
            }
            Command::Rm { track } => {
                let t = self.db().track_by_id(track)?.with_context(|| format!("no track with id {track}"))?;
                let playlists = self.db().playlists_with_track(track)?;
                if let Some(rel) = &t.path {
                    if self.db().tracks_with_path(rel)?.len() > 1 {
                        info(tx, format!("another track still uses {rel}; the file stays")).await?;
                    } else {
                        library::remove_track(&self.config.library, Path::new(rel))?;
                    }
                }
                self.db().delete_track(track)?;
                self.rewrite_m3us(Some(&playlists))?;
                info(tx, format!("removed {} - {}", t.artist, t.title)).await
            }
            Command::Source { track, url } => {
                ensure!(self.db().set_source_url(track, &url)?, "no track with id {track}");
                info(tx, format!("track {track} queued for download from {url}")).await
            }
            Command::Refile { dry_run } => self.refile(dry_run, tx).await,
            Command::Remote { action } => self.remote(action, tx).await,
            Command::Import { dir, apply, r#move, copy: _, delete_duplicates } => {
                self.import(&dir, apply, !r#move, delete_duplicates, tx).await
            }
        }
    }

    /// Re-resolve every track against MusicBrainz (new ranking, new fields), then retag and
    /// move files whose templated path changed.
    async fn refile(&self, dry_run: bool, tx: &Sender<Event>) -> Result<()> {
        let cfg = &self.config;
        let tracks = self.db().all_tracks()?;
        let total = tracks.len();
        let (mut moved, mut failed) = (0, 0);
        for (i, t) in tracks.iter().enumerate() {
            progress(tx, format!("checking {}/{total}: {} - {}", i + 1, t.artist, t.title)).await?;
            let r = match self.mb.recording(&t.recording_mbid).await {
                Ok(r) => r,
                Err(e) => {
                    info(tx, format!("skipping {} - {}: {e:#}", t.artist, t.title)).await?;
                    failed += 1;
                    continue;
                }
            };
            if !dry_run {
                self.db().insert_track(&new_track(&t.recording_mbid, &r))?;
            }
            let Some(old_rel) = &t.path else { continue };
            // What the row looks like after the update, without a second query in dry-run mode.
            let fresh = match dry_run {
                true => Track {
                    title: r.title.clone(),
                    artist: r.artist.clone(),
                    album_artist: r.album_artist.clone(),
                    album: r.album.clone(),
                    year: r.year,
                    track_no: r.track_no,
                    ..t.clone()
                },
                false => self.db().track_by_mbid(&t.recording_mbid)?.context("track vanished")?,
            };
            let ext = Path::new(old_rel).extension().and_then(|e| e.to_str()).unwrap_or("bin");
            let new_rel = library::render_template(&cfg.path_template, &fresh, ext)?;
            if new_rel != Path::new(old_rel) {
                info(tx, format!("  {old_rel} → {}", new_rel.display())).await?;
                moved += 1;
                if !dry_run {
                    library::move_track(&cfg.library, Path::new(old_rel), &new_rel)?;
                    self.db().set_path(t.id, &new_rel.to_string_lossy())?;
                }
            }
            if !dry_run {
                library::tag(&cfg.library.join(&new_rel), &fresh)?;
            }
        }
        if !dry_run {
            // Paths inside the m3u8 files may have changed; regenerate every one we wrote.
            self.rewrite_m3us(None)?;
        }
        let verb = if dry_run { "would move" } else { "moved" };
        info(tx, format!("{total} tracks checked, {moved} {verb}, {failed} failed")).await?;
        ensure!(failed == 0, "{failed} MusicBrainz lookups failed; run again later");
        Ok(())
    }

    async fn remote(&self, action: RemoteCmd, tx: &Sender<Event>) -> Result<()> {
        // Backups are written under the playlist folder; bring an old layout over first.
        self.migrate_playlist_dir(tx).await?;
        match action {
            RemoteCmd::Ls { to } => {
                for target in self.targets(to.as_deref())? {
                    let playlists = target.list_playlists().await?;
                    data(tx, json!({ "target": target.name(), "playlists": playlists })).await?;
                }
            }
            RemoteCmd::Rm { pattern, to, yes, include_smart, no_backup, out } => {
                for target in self.targets(to.as_deref())? {
                    let hits: Vec<_> = target
                        .list_playlists()
                        .await?
                        .into_iter()
                        .filter(|p| glob_match(&pattern, &p.title) && (include_smart || !p.smart))
                        .collect();
                    if !yes {
                        info(
                            tx,
                            format!(
                                "{}: {} playlists match {pattern:?} (pass --yes to delete):",
                                target.name(),
                                hits.len()
                            ),
                        )
                        .await?;
                        for p in &hits {
                            info(tx, format!("  {} ({} tracks)", p.title, p.tracks)).await?;
                        }
                        continue;
                    }
                    if !no_backup {
                        let dir = out.clone().unwrap_or_else(|| self.config.playlist_dir().join("backup"));
                        self.backup(target, &hits, &dir, tx).await?;
                    }
                    for p in &hits {
                        target.delete_playlist(&p.id).await?;
                        info(tx, format!("{}: deleted {} ({} tracks)", target.name(), p.title, p.tracks)).await?;
                    }
                }
            }
            RemoteCmd::Backup { pattern, to, out } => {
                let dir = out.unwrap_or_else(|| self.config.playlist_dir().join("backup"));
                for target in self.targets(to.as_deref())? {
                    let hits: Vec<_> =
                        target.list_playlists().await?.into_iter().filter(|p| glob_match(&pattern, &p.title)).collect();
                    self.backup(target, &hits, &dir, tx).await?;
                }
            }
            RemoteCmd::Relink { dir, from } => {
                let dir = dir.unwrap_or_else(|| self.config.playlist_dir().join("backup"));
                let (map, unresolved) = self.relink_map(&dir, &from, tx).await?;
                let n = rewrite_backups(&dir, &map)?;
                info(tx, format!("{} paths relinked to the library in {n} backup files; {unresolved} outside paths have no library copy", map.len())).await?;
            }
            RemoteCmd::Restore { file, to } => {
                let saved: Value = serde_json::from_slice(&std::fs::read(&file)?).context("reading backup")?;
                ensure!(
                    saved["smart"] != true,
                    "smart playlists are filters, not lists; recreate it in the server's UI"
                );
                let title = saved["title"].as_str().context("backup has no title")?;
                let name = to.as_deref().or(saved["target"].as_str());
                let files: Vec<PathBuf> = saved["items"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|i| i["file"].as_str().map(PathBuf::from))
                    .collect();
                ensure!(!files.is_empty(), "backup has no file paths to restore from");
                for target in self.targets(name)? {
                    let id = target.sync_playlist(title, &files).await?;
                    info(tx, format!("{}: restored {title} ({} tracks, id {id})", target.name(), files.len())).await?;
                }
            }
        }
        Ok(())
    }

    /// One m3u8 (local paths) plus a JSON sidecar per playlist, under `<dir>/<target>/`.
    async fn backup(
        &self,
        target: &dyn SyncTarget,
        hits: &[RemotePlaylist],
        dir: &Path,
        tx: &Sender<Event>,
    ) -> Result<()> {
        let dir = dir.join(target.name());
        std::fs::create_dir_all(&dir)?;
        // Plex allows two playlists with one title; the second gets its id in the file name.
        let mut used = HashSet::new();
        for p in hits {
            let items = target.playlist_items(&p.id).await?;
            let mut stem = m3u::slug(&p.title);
            if !used.insert(stem.clone()) {
                stem = format!("{stem}-{}", m3u::slug(&p.id));
                used.insert(stem.clone());
            }
            let entries: Vec<(String, PathBuf)> = items
                .iter()
                .filter_map(|i| i.file.clone().map(|f| (format!("{} - {}", i.artist, i.title), f)))
                .collect();
            let m3u = dir.join(format!("{stem}.m3u8"));
            m3u::write_entries(&m3u, &p.title, &entries)?;
            let side = json!({ "target": target.name(), "id": p.id, "title": p.title, "smart": p.smart, "tracks": p.tracks, "items": items });
            std::fs::write(dir.join(format!("{stem}.json")), serde_json::to_vec_pretty(&side)?)?;
            info(
                tx,
                format!("{}: backed up {} ({} tracks) → {}", target.name(), p.title, entries.len(), m3u.display()),
            )
            .await?;
        }
        Ok(())
    }

    /// Resolve files from elsewhere to recordings; file one copy of each new recording, report
    /// (or delete) the rest. Backups under `<playlist_dir>/backup` follow the files that moved.
    async fn import(
        &self,
        dir: &Path,
        apply: bool,
        copy: bool,
        delete_duplicates: bool,
        tx: &Sender<Event>,
    ) -> Result<()> {
        let cfg = &self.config;
        if apply {
            // The backups this run may rewrite live under the playlist folder.
            self.migrate_playlist_dir(tx).await?;
        }
        let found = library::scan(dir)?;
        info(tx, format!("{} audio files under {}", found.len(), dir.display())).await?;
        if apply && !copy {
            // Moving and tagging need write access to the source; find out before the long pass.
            let unwritable = found.iter().filter(|f| !library::writable(&f.path)).count();
            ensure!(
                unwritable == 0,
                "source not writable: {unwritable} of {} files (owned by another user?); drop --move to copy instead",
                found.len()
            );
        }
        let verb = if copy { "copied" } else { "imported" };
        let (mut unresolved, mut lookup_failed) = (0, 0);

        // Pass 1: every file to a recording MBID, grouped in first-seen order. Explo drops the
        // same track into every playlist folder it appears in, so groups are the norm.
        let mut groups: Vec<(String, Vec<&library::Found>)> = vec![];
        let mut index: HashMap<String, usize> = HashMap::new();
        // Files the search could not place; `None` is no match, `Some` a failed lookup.
        let mut pending: Vec<(&library::Found, Option<anyhow::Error>)> = vec![];
        let (mut by_tag, mut by_search) = (0, 0);
        for (i, f) in found.iter().enumerate() {
            progress(
                tx,
                format!(
                    "resolving {}/{} (tag {by_tag}, search {by_search}, unplaced {})",
                    i + 1,
                    found.len(),
                    pending.len()
                ),
            )
            .await?;
            let path_s = f.path.to_string_lossy().into_owned();
            let cached = match &f.recording_mbid {
                Some(_) => None,
                None => self.db().cached_lookup(&path_s, f.size, f.mtime)?,
            };
            let mbid = match (&f.recording_mbid, cached) {
                (Some(m), _) => {
                    by_tag += 1;
                    m.clone()
                }
                (None, Some(m)) => {
                    by_search += 1;
                    m
                }
                (None, None) => match self.mb.search_recording(&f.artist, &f.title, f.duration_ms).await {
                    Ok(Some(m)) => {
                        by_search += 1;
                        self.db().cache_lookup(&path_s, f.size, f.mtime, &m)?;
                        m
                    }
                    Ok(None) => {
                        pending.push((f, None));
                        continue;
                    }
                    Err(e) => {
                        pending.push((f, Some(e)));
                        continue;
                    }
                },
            };
            match index.get(&mbid) {
                Some(&i) => groups[i].1.push(f),
                None => {
                    index.insert(mbid.clone(), groups.len());
                    groups.push((mbid, vec![f]));
                }
            }
        }
        // An untagged copy of a file that did resolve (same artist and title, same length
        // within a few seconds) joins that group instead of being a miss.
        for (f, err) in pending {
            let twin = groups.iter().position(|(_, files)| {
                files.iter().any(|g| {
                    name_key(g) == name_key(f)
                        && matches!((g.duration_ms, f.duration_ms), (Some(a), Some(b)) if (a - b).abs() <= 5_000)
                })
            });
            match (twin, err) {
                (Some(i), _) => {
                    info(tx, format!("matched by name: {} ~ {}", f.path.display(), groups[i].1[0].path.display()))
                        .await?;
                    groups[i].1.push(f);
                }
                (None, None) => {
                    info(tx, format!("no match: {}", f.path.display())).await?;
                    unresolved += 1;
                }
                (None, Some(e)) => {
                    info(tx, format!("lookup failed: {}: {e:#}", f.path.display())).await?;
                    lookup_failed += 1;
                }
            }
        }

        // Pass 2: per recording, the library's copy wins; else the file closest to the
        // recording's length is filed and the others are duplicates.
        let (mut imported, mut duplicates, mut failed) = (0, 0, 0);
        let mut moved: HashMap<PathBuf, PathBuf> = HashMap::new();
        for (mbid, files) in groups {
            let existing = self.db().track_by_mbid(&mbid)?;
            let (kept, label, dest) = match existing {
                Some(t) if t.status == "ready" => {
                    (None, format!("{} - {}", t.artist, t.title), t.path.map(|p| cfg.library.join(p)))
                }
                _ => {
                    let r = match self.mb.recording(&mbid).await {
                        Ok(r) => r,
                        Err(e) => {
                            for f in &files {
                                info(tx, format!("lookup failed: {}: {e:#}", f.path.display())).await?;
                            }
                            lookup_failed += files.len();
                            continue;
                        }
                    };
                    let drift = |f: &&&library::Found| match (f.duration_ms, r.length_ms) {
                        (Some(a), Some(b)) => (a - b).abs(),
                        _ => i64::MAX,
                    };
                    let best = *files.iter().min_by_key(drift).expect("group has a file");
                    info(tx, format!("{verb}: {} → {} - {} [{}]", best.path.display(), r.artist, r.title, r.album))
                        .await?;
                    let mut dest = None;
                    if apply {
                        // The row exists only once the file is in place: a failed move or tag
                        // must not leave a `wanted` track for the next sync to download.
                        // Another recording may already sit at the templated path ("End of You",
                        // two recordings on one single). Same audio: this one is its duplicate.
                        // Different audio: file_track picks a " (2)" name.
                        let ext = best.path.extension().and_then(|e| e.to_str()).unwrap_or("bin");
                        let probe = Track {
                            title: r.title.clone(),
                            artist: r.artist.clone(),
                            album_artist: r.album_artist.clone(),
                            album: r.album.clone(),
                            year: r.year,
                            track_no: r.track_no,
                            ..Track::default()
                        };
                        let planned = library::render_template(&cfg.path_template, &probe, ext)?;
                        let at = cfg.library.join(&planned);
                        if at.exists() && library::same_audio(&best.path, &at) {
                            let twin = self.db().tracks_with_path(&planned.to_string_lossy())?.into_iter().next();
                            let label = twin
                                .map(|t| format!("{} - {} (same file)", t.artist, t.title))
                                .unwrap_or_else(|| planned.display().to_string());
                            for f in &files {
                                duplicates += 1;
                                moved.insert(f.path.clone(), at.clone());
                                info(tx, format!("duplicate of {label}: {}", f.path.display())).await?;
                            }
                            continue;
                        }
                        let had_row = existing.is_some();
                        let id = self.db().insert_track(&new_track(&mbid, &r))?;
                        let t = self.db().track_by_mbid(&mbid)?.context("track vanished")?;
                        match library::file_track(&cfg.library, &cfg.path_template, &t, &best.path, copy) {
                            Ok(rel) => {
                                self.db().mark_ready(id, &rel.to_string_lossy(), None)?;
                                let d = cfg.library.join(rel);
                                moved.insert(best.path.clone(), d.clone());
                                dest = Some(d);
                            }
                            Err(e) => {
                                if !had_row {
                                    self.db().delete_track(id)?;
                                }
                                info(tx, format!("failed: {}: {e:#}", best.path.display())).await?;
                                failed += 1;
                                // Its copies stay where they are too; nothing landed.
                                continue;
                            }
                        }
                    }
                    imported += 1;
                    (Some(&best.path), format!("{} - {}", r.artist, r.title), dest)
                }
            };
            for f in files.iter().filter(|f| Some(&f.path) != kept) {
                duplicates += 1;
                // Backups follow every copy to the kept file, moved, copied or deleted alike:
                // the source folder is on its way out either way.
                if let Some(d) = &dest
                    && apply
                {
                    moved.insert(f.path.clone(), d.clone());
                }
                if apply && delete_duplicates {
                    std::fs::remove_file(&f.path)?;
                    info(tx, format!("deleted duplicate of {label}: {}", f.path.display())).await?;
                } else {
                    info(tx, format!("duplicate of {label}: {}", f.path.display())).await?;
                }
            }
        }
        if !moved.is_empty() {
            let n = rewrite_backups(&cfg.playlist_dir().join("backup"), &moved)?;
            if n > 0 {
                info(tx, format!("updated {n} backup files to point at the moved tracks")).await?;
            }
        }
        let mode = match (apply, copy) {
            (true, _) => "",
            (false, true) => " (dry run; pass --apply to copy files in)",
            (false, false) => " (dry run; pass --apply --move to move files in)",
        };
        info(
            tx,
            format!(
                "{imported} {verb}, {duplicates} duplicates, {unresolved} no match, {lookup_failed} lookups failed{}{mode}",
                if failed > 0 { format!(", {failed} failed") } else { String::new() }
            ),
        )
        .await?;
        ensure!(failed == 0, "{failed} files could not be {verb}; see the lines above");
        ensure!(
            lookup_failed == 0,
            "{lookup_failed} MusicBrainz lookups failed; run import again later for those files"
        );
        Ok(())
    }

    /// For `remote relink`: every distinct path in the backups that is not under the library
    /// (and under one of `from`, when given), mapped to the library file of the same recording.
    /// One tag read per distinct file, against an in-memory index of the library; smart
    /// playlists' backups are skipped, Plex regenerates those. Returns the map and how many
    /// paths stay unresolved.
    async fn relink_map(
        &self,
        dir: &Path,
        from: &[PathBuf],
        tx: &Sender<Event>,
    ) -> Result<(HashMap<PathBuf, PathBuf>, usize)> {
        let library = &self.config.library;
        let mut outside: Vec<PathBuf> = vec![];
        for p in backup_files(dir)?.into_iter().filter(|p| p.extension().is_some_and(|e| e == "m3u8")) {
            for line in std::fs::read_to_string(&p)?.lines() {
                let path = Path::new(line);
                if line.starts_with('/')
                    && !path.starts_with(library)
                    && (from.is_empty() || from.iter().any(|f| path.starts_with(f)))
                {
                    outside.push(path.to_path_buf());
                }
            }
        }
        outside.sort();
        outside.dedup();

        // The library, indexed once: by recording id, and by name + length for untagged copies.
        let all = self.db().all_tracks()?;
        let by_mbid: HashMap<&str, &str> =
            all.iter().filter_map(|t| Some((t.recording_mbid.as_str(), t.path.as_deref()?))).collect();
        let by_name: Vec<(String, String, i64, &str)> = all
            .iter()
            .filter_map(|t| {
                Some((
                    crate::musicbrainz::strip_feat(&t.artist).to_lowercase(),
                    crate::musicbrainz::strip_feat(&t.title).to_lowercase(),
                    t.duration_ms?,
                    t.path.as_deref()?,
                ))
            })
            .collect();

        let mut map = HashMap::new();
        let mut unresolved = 0;
        let total = outside.len();
        for (i, path) in outside.into_iter().enumerate() {
            progress(tx, format!("resolving {}/{total}: {}", i + 1, path.display())).await?;
            let found = library::inspect(&path);
            let mbid = match &found.recording_mbid {
                Some(m) => Some(m.clone()),
                None => self.db().cached_lookup(&path.to_string_lossy(), found.size, found.mtime)?,
            };
            let hit = mbid.as_deref().and_then(|m| by_mbid.get(m).copied()).or_else(|| {
                // Credits differ in form ("Clean Bandit" vs "Clean Bandit feat. Demi Lovato"),
                // so one artist string starting with the other is a match.
                let (artist, title) = name_key(&found);
                let len = found.duration_ms?;
                by_name
                    .iter()
                    .find(|(a, t, ms, _)| {
                        (a.starts_with(&artist) || artist.starts_with(a)) && *t == title && (ms - len).abs() <= 5_000
                    })
                    .map(|(_, _, _, p)| *p)
            });
            match hit {
                Some(rel) => {
                    map.insert(path, library.join(rel));
                }
                None => unresolved += 1,
            }
        }
        Ok((map, unresolved))
    }

    /// The archive stem of a playlist: ISO week or date. The title's date ("week of 2026-10-05",
    /// "2026-10-17 Sat") is what the user knows the playlist by; ListenBrainz's own date field
    /// can be the day before.
    fn period_of(&self, p: &Playlist) -> String {
        let date = date_in(&p.name)
            .or_else(|| p.date.clone().filter(|d| !d.is_empty()))
            .unwrap_or_else(|| p.fetched_at.get(..10).unwrap_or("undated").to_string());
        m3u::period(&p.kind, &date)
    }

    /// Write a superseded playlist to `archive/<kind>/<period>.m3u8` and drop its old files.
    fn archive(&self, p: &Playlist) -> Result<PathBuf> {
        let cfg = &self.config;
        let dir = cfg.playlist_dir().join("archive").join(&p.kind);
        let tracks = self.db().playlist_tracks(p.id)?;
        let file = m3u::write(&dir, &cfg.library, &self.period_of(p), &p.name, &tracks)?;
        self.remove_stale(p, &file);
        Ok(file)
    }

    /// Delete the files earlier naming schemes left for this playlist, other than `keep`: the
    /// path recorded in the database, the ListenBrainz title, "<Name> <date>" and "<Name>".
    fn remove_stale(&self, p: &Playlist, keep: &Path) {
        let dir = self.config.playlist_dir();
        let current = dir.join(format!("{}.m3u8", p.kind));
        let mut old: Vec<PathBuf> = vec![
            dir.join(format!("{}.m3u8", m3u::safe_name(&p.name))),
            dir.join(format!("{}.m3u8", m3u::safe_name(&self.title(&p.kind)))),
        ];
        old.extend(p.m3u_path.as_deref().map(PathBuf::from));
        for d in [p.date.clone(), date_in(&p.name)].into_iter().flatten().filter(|d| !d.is_empty()) {
            old.push(dir.join(format!("{} {d}.m3u8", display_name(&p.kind))));
        }
        // Never the file just written, nor the kind's current file: a superseded row's m3u_path
        // still points at it until the current row is rewritten a moment later.
        for f in old.into_iter().filter(|f| *f != keep && *f != current) {
            let _ = std::fs::remove_file(&f);
        }
    }

    /// Files from earlier naming schemes whose playlist is no longer in the database
    /// ("Daily Jams 2026-10-16.m3u8", "Daily Jams for rob, 2026-10-16 Thu.m3u8") still get
    /// archived: moved two levels down with their relative paths adjusted, or dropped when the
    /// archive already has that period.
    fn archive_legacy(&self, kind: &str, current: &Path, current_period: Option<&str>) -> Result<()> {
        let dir = self.config.playlist_dir();
        let prefix = format!("{} ", display_name(kind));
        let archive = dir.join("archive").join(kind);
        let Ok(entries) = std::fs::read_dir(&dir) else { return Ok(()) };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(date) = name.strip_suffix(".m3u8").filter(|_| name.starts_with(&prefix)).and_then(date_in) else {
                continue;
            };
            if path == current {
                continue;
            }
            let period = m3u::period(kind, &date);
            if current_period == Some(period.as_str()) {
                // Same period as the current playlist: an earlier edition, superseded, not history.
                std::fs::remove_file(&path)?;
                continue;
            }
            let target = archive.join(format!("{period}.m3u8"));
            if !target.exists() {
                std::fs::create_dir_all(&archive)?;
                let body: String = std::fs::read_to_string(&path)?
                    .lines()
                    .map(|l| match l.starts_with(['#', '/']) || l.is_empty() {
                        true => l.to_string(),
                        false => format!("../../{l}"),
                    })
                    .map(|l| l + "\n")
                    .collect();
                std::fs::write(&target, body)?;
            }
            std::fs::remove_file(&path)?;
        }
        Ok(())
    }

    /// Bring an older playlist folder to the current layout, moving and never deleting:
    /// `Playlists/` → `playlists/` (when the default is in use), backup files to slug names,
    /// dated and long-titled m3u8s into the archive, database paths along with them.
    async fn migrate_playlist_dir(&self, tx: &Sender<Event>) -> Result<()> {
        let cfg = &self.config;
        let dir = cfg.playlist_dir();
        if cfg.playlist_dir.is_none()
            && let Some(parent) = dir.parent()
            && let Ok(entries) = std::fs::read_dir(parent)
        {
            // Literal names: on a case-insensitive filesystem `exists()` cannot tell them apart.
            let names: Vec<String> = entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
            if names.iter().any(|n| n == "Playlists") && !names.iter().any(|n| n == "playlists") {
                let old = parent.join("Playlists");
                std::fs::rename(&old, &dir)?;
                info(tx, format!("moved {} → {}", old.display(), dir.display())).await?;
                let (old_s, new_s) = (old.to_string_lossy().into_owned(), dir.to_string_lossy().into_owned());
                let playlists = self.db().playlists()?;
                for p in playlists {
                    if let Some(rest) = p.m3u_path.as_deref().and_then(|m| m.strip_prefix(old_s.as_str())) {
                        self.db().set_playlist_m3u(p.id, &format!("{new_s}{rest}"))?;
                    }
                }
            }
        }
        if let Ok(targets) = std::fs::read_dir(dir.join("backup")) {
            for t in targets.flatten().filter(|t| t.path().is_dir()) {
                // Rename pairs by stem, never over an existing file. "Weekly Jams" and "Weekly-Jams"
                // both slug to weekly-jams; the second pair gets the remote id from its sidecar,
                // like fresh backups do. Literal names: a case-insensitive filesystem would say the
                // slug "exists" when only the differently-cased original does.
                let mut names: HashSet<String> = std::fs::read_dir(t.path())?
                    .flatten()
                    .map(|f| f.file_name().to_string_lossy().into_owned())
                    .collect();
                let mut stems: Vec<String> =
                    names.iter().filter_map(|n| n.rsplit_once('.').map(|(s, _)| s.to_string())).collect();
                stems.sort();
                stems.dedup();
                for stem in stems {
                    let slug = m3u::slug(&stem);
                    if slug == stem {
                        continue;
                    }
                    let exts = ["json", "m3u8"];
                    // Case-insensitively, since "Weekly-Jams" and "weekly-jams" are one file on
                    // macOS; a stem's own files don't count against it (a case-only rename).
                    let taken = |s: &str| {
                        names.iter().any(|n| {
                            !n.starts_with(&format!("{stem}."))
                                && exts.iter().any(|e| n.eq_ignore_ascii_case(&format!("{s}.{e}")))
                        })
                    };
                    let target = if !taken(&slug) {
                        slug.clone()
                    } else {
                        let side = std::fs::read(t.path().join(format!("{stem}.json"))).ok();
                        let id = side
                            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                            .and_then(|v| v["id"].as_str().map(m3u::slug))
                            .unwrap_or_default();
                        let with_id = format!("{slug}-{id}");
                        if id.is_empty() || taken(&with_id) {
                            info(tx, format!("backup {stem:?} left as is: {slug:?} already exists")).await?;
                            continue;
                        }
                        with_id
                    };
                    for e in exts {
                        let from = format!("{stem}.{e}");
                        if names.contains(&from) {
                            std::fs::rename(t.path().join(&from), t.path().join(format!("{target}.{e}")))?;
                            names.remove(&from);
                            names.insert(format!("{target}.{e}"));
                            info(tx, format!("backup renamed {from} → {target}.{e}")).await?;
                        }
                    }
                }
            }
        }
        // Rows from before the date column: take the date from the title, as the archive does.
        // Bind first: a guard in the `for` expression would live across the loop and deadlock.
        let playlists = self.db().playlists()?;
        for p in playlists {
            if p.date.as_deref().unwrap_or("").is_empty()
                && let Some(d) = date_in(&p.name)
            {
                self.db().set_playlist_date(p.id, &d)?;
            }
        }
        let playlists = self.db().playlists()?;
        let mut kinds: Vec<String> = playlists.iter().map(|p| p.kind.clone()).collect();
        kinds.extend(cfg.listenbrainz.playlists.iter().cloned());
        kinds.sort();
        kinds.dedup();
        for kind in kinds {
            let current = playlists.iter().find(|p| p.kind == kind).map(|p| self.period_of(p));
            self.archive_legacy(&kind, &dir.join(format!("{kind}.m3u8")), current.as_deref())?;
        }
        Ok(())
    }

    /// The playlist's name on the targets, from `sync.playlist_title`.
    fn title(&self, kind: &str) -> String {
        self.config.sync.playlist_title.replace("{name}", &display_name(kind)).replace("{kind}", kind)
    }

    /// Rewrite the m3u8 of every playlist (or of `only` those ids) at its stored path.
    fn rewrite_m3us(&self, only: Option<&[i64]>) -> Result<()> {
        let playlists = self.db().playlists()?;
        for p in playlists.iter().filter(|p| only.is_none_or(|ids| ids.contains(&p.id))) {
            let Some(m3u) = p.m3u_path.as_deref().map(Path::new) else { continue };
            let (Some(dir), Some(stem)) = (m3u.parent(), m3u.file_stem().and_then(|s| s.to_str())) else { continue };
            let tracks = self.db().playlist_tracks(p.id)?;
            m3u::write(dir, &self.config.library, stem, &p.name, &tracks)?;
        }
        Ok(())
    }

    fn targets(&self, only: Option<&str>) -> Result<Vec<&dyn SyncTarget>> {
        let v: Vec<&dyn SyncTarget> =
            self.targets.iter().map(|t| t.as_ref()).filter(|t| only.is_none_or(|o| o == t.name())).collect();
        ensure!(
            !v.is_empty(),
            "no sync target{} configured",
            only.map(|o| format!(" named {o:?}")).unwrap_or_default()
        );
        Ok(v)
    }

    async fn sync(&self, only: Option<String>, no_push: bool, no_fetch: bool, tx: &Sender<Event>) -> Result<()> {
        let cfg = &self.config;

        // 1. ListenBrainz playlists → DB rows (new tracks resolved through MusicBrainz).
        let mut fetched: Vec<i64> = vec![];
        let lb_playlists = if no_fetch { vec![] } else { self.lb.created_for(&cfg.listenbrainz.user).await? };
        for lb in listenbrainz::newest_per_kind(lb_playlists) {
            if !cfg.listenbrainz.playlists.contains(&lb.kind) || only.as_ref().is_some_and(|k| *k != lb.kind) {
                continue;
            }
            info(tx, format!("fetching {} ({})", lb.title, lb.kind)).await?;
            let tracks = self.lb.tracks(&lb.mbid).await?;
            let total = tracks.len();
            let mut ids = vec![];
            for (i, t) in tracks.iter().enumerate() {
                let existing = self.db().track_by_mbid(&t.recording_mbid)?;
                let id = match existing {
                    Some(tr) => tr.id,
                    None => {
                        info(tx, format!("resolving {}/{total}: {} - {}", i + 1, t.artist, t.title)).await?;
                        let r = self.mb.recording(&t.recording_mbid).await;
                        let r = match r {
                            Ok(r) => r,
                            Err(e) => {
                                info(tx, format!("skipping {} - {}: {e:#}", t.artist, t.title)).await?;
                                continue;
                            }
                        };
                        self.db().insert_track(&new_track(&t.recording_mbid, &r))?
                    }
                };
                ids.push(id);
            }
            let pid = self.db().upsert_playlist(&lb.mbid, &lb.title, &lb.kind, lb.date.get(..10).unwrap_or(""))?;
            self.db().set_playlist_tracks(pid, &ids)?;
            fetched.push(pid);
        }

        // 2. Download whatever is still wanted, tag it, file it.
        let wanted = self.db().wanted_tracks()?;
        info(tx, format!("{} tracks to download", wanted.len())).await?;
        let (mut done, mut failed) = (0, 0);
        let mut done_ids = vec![];
        for t in &wanted {
            info(tx, format!("downloading {} - {}", t.artist, t.title)).await?;
            let result = async {
                let got = self.downloader.download(t, &cfg.tmp_dir()).await?;
                let rel = library::file_track(&cfg.library, &cfg.path_template, t, &got.path, false)?;
                Ok::<_, anyhow::Error>((rel, got.source_url))
            }
            .await;
            match result {
                Ok((rel, url)) => {
                    self.db().mark_ready(t.id, &rel.to_string_lossy(), url.as_deref())?;
                    done += 1;
                    done_ids.push(t.id);
                }
                Err(e) => {
                    self.db().mark_failed(t.id, &format!("{e:#}"))?;
                    info(tx, format!("failed {} - {}: {e:#}", t.artist, t.title)).await?;
                    failed += 1;
                }
            }
        }

        // 3. One current m3u8 per kind ("weekly-jams.m3u8"), replaced on every run; the
        // playlists it replaces move to archive/<kind>/<period>.m3u8. Then push the current one.
        // Kinds touched: those fetched above, plus any whose playlist just gained a track (the
        // `--no-fetch` case).
        self.migrate_playlist_dir(tx).await?;
        let playlists = self.db().playlists()?;
        let mut kinds: Vec<&str> = vec![];
        for p in &playlists {
            let gained = || self.db().playlist_tracks(p.id).map(|ts| ts.iter().any(|t| done_ids.contains(&t.id)));
            if (fetched.contains(&p.id) || gained()?) && !kinds.contains(&p.kind.as_str()) {
                kinds.push(&p.kind);
            }
        }
        for kind in kinds {
            // `playlists()` is newest first, so the first row of a kind is the current one.
            let mut rows = playlists.iter().filter(|p| p.kind == kind);
            let Some(cur) = rows.next() else { continue };
            let tracks = self.db().playlist_tracks(cur.id)?;
            let file = m3u::write(&cfg.playlist_dir(), &cfg.library, kind, &cur.name, &tracks)?;
            self.remove_stale(cur, &file);
            for old in rows {
                if self.period_of(old) == self.period_of(cur) {
                    // ListenBrainz regenerated the same week: an earlier edition, not history.
                    // The current file already covers this period; drop the old one's files.
                    self.remove_stale(old, &file);
                    self.db().clear_playlist_m3u(old.id)?;
                    continue;
                }
                let archived = self.archive(old)?;
                self.db().set_playlist_m3u(old.id, &archived.to_string_lossy())?;
            }
            self.db().set_playlist_m3u(cur.id, &file.to_string_lossy())?;
            info(tx, format!("wrote {}", file.display())).await?;
            if !no_push {
                self.push(cur, None, tx).await?;
            }
        }
        info(tx, format!("{done} downloaded, {failed} failed")).await?;
        ensure!(failed == 0, "{failed} downloads failed; `td retry` queues them again, `td source` pins a URL");
        Ok(())
    }

    async fn push(&self, p: &Playlist, only: Option<&str>, tx: &Sender<Event>) -> Result<()> {
        let files: Vec<PathBuf> = self
            .db()
            .playlist_tracks(p.id)?
            .iter()
            .filter_map(|t| t.path.as_ref().map(|rel| self.config.library.join(rel)))
            .collect();
        let playlists = self.db().playlists()?;
        let title = self.remote_title(p, &playlists);
        self.replace_remote(&title, &files, Some(p.id), only, tx).await
    }

    /// Push any m3u8 file. Lines are resolved against the file's folder; what is not on disk is
    /// reported and skipped rather than failing the push.
    async fn push_file(
        &self,
        file: &Path,
        title: Option<String>,
        only: Option<&str>,
        tx: &Sender<Event>,
    ) -> Result<()> {
        let title = match title {
            Some(t) => t,
            None => self.file_title(file)?,
        };
        let base = file.parent().unwrap_or(Path::new("."));
        let mut files = vec![];
        for line in
            std::fs::read_to_string(file)?.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#'))
        {
            let path = base.join(line);
            if path.is_file() {
                files.push(path);
            } else {
                info(tx, format!("missing, skipped: {line}")).await?;
            }
        }
        ensure!(!files.is_empty(), "{} has no tracks that exist on disk", file.display());
        let playlists = self.db().playlists()?;
        let id = playlists.iter().find(|p| p.m3u_path.as_deref() == file.to_str()).map(|p| p.id);
        self.replace_remote(&title, &files, id, only, tx).await
    }

    /// Back up a same-titled remote playlist, then create or replace it. `row` records the sync.
    async fn replace_remote(
        &self,
        title: &str,
        files: &[PathBuf],
        row: Option<i64>,
        only: Option<&str>,
        tx: &Sender<Event>,
    ) -> Result<()> {
        for target in self.targets.iter().filter(|t| only.is_none_or(|o| o == t.name())) {
            if let Ok(list) = target.list_playlists().await {
                let same: Vec<_> = list.into_iter().filter(|p| p.title == title && !p.smart).collect();
                if !same.is_empty() {
                    self.backup(target.as_ref(), &same, &self.config.playlist_dir().join("backup"), tx).await?;
                }
            }
            match target.sync_playlist(title, files).await {
                Ok(remote) => {
                    if let Some(id) = row {
                        self.db().record_sync(id, target.name(), &remote)?;
                    }
                    info(tx, format!("{}: pushed {title} ({} tracks)", target.name(), files.len())).await?;
                }
                Err(e) => info(tx, format!("{}: push of {title} failed: {e:#}", target.name())).await?,
            }
        }
        Ok(())
    }

    /// What a playlist row is called on the targets: the kind's title for the current playlist
    /// of each kind, the full ListenBrainz title for an archived one.
    fn remote_title(&self, p: &Playlist, all: &[Playlist]) -> String {
        remote_title(&self.config, p, all)
    }

    /// Title for an m3u8 file: its database row's remote title, else its #PLAYLIST line, else
    /// the file name.
    fn file_title(&self, file: &Path) -> Result<String> {
        let playlists = self.db().playlists()?;
        if let Some(p) = playlists.iter().find(|p| p.m3u_path.as_deref() == file.to_str()) {
            return Ok(self.remote_title(p, &playlists));
        }
        let body = std::fs::read_to_string(file)?;
        Ok(body
            .lines()
            .find_map(|l| l.strip_prefix("#PLAYLIST:"))
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| file.file_stem().unwrap_or_default().to_string_lossy().into_owned()))
    }

    fn db(&self) -> std::sync::MutexGuard<'_, Db> {
        self.db.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// What two copies of one song look like from their tags or file names.
fn name_key(f: &library::Found) -> (String, String) {
    (f.artist.to_lowercase(), crate::musicbrainz::strip_feat(&f.title).to_lowercase())
}

/// Point backup m3u8/json files under `dir` at the new locations of moved (or deleted-as-duplicate)
/// files, so `td remote restore` keeps working after an import. Returns how many files changed.
fn rewrite_backups(dir: &Path, moved: &HashMap<PathBuf, PathBuf>) -> Result<usize> {
    let mut changed = 0;
    for p in backup_files(dir)? {
        match p.extension().and_then(|e| e.to_str()) {
            Some("m3u8") => {
                let body = std::fs::read_to_string(&p)?;
                let new: String = body
                    .lines()
                    .map(|l| moved.get(Path::new(l)).map(|n| n.to_string_lossy().into_owned()).unwrap_or(l.into()))
                    .map(|l| l + "\n")
                    .collect();
                if new != body {
                    write_atomic(&p, new.as_bytes())?;
                    changed += 1;
                }
            }
            Some("json") => {
                let mut v: Value = serde_json::from_slice(&std::fs::read(&p)?)?;
                let mut hit = false;
                for item in v["items"].as_array_mut().into_iter().flatten() {
                    if let Some(n) = item["file"].as_str().and_then(|f| moved.get(Path::new(f))) {
                        item["file"] = json!(n);
                        hit = true;
                    }
                }
                if hit {
                    write_atomic(&p, &serde_json::to_vec_pretty(&v)?)?;
                    changed += 1;
                }
            }
            _ => {}
        }
    }
    Ok(changed)
}

/// Every backup m3u8/json under `dir`, minus smart playlists (their JSON says `"smart": true`,
/// and the m3u8 beside it shares the stem): Plex regenerates those from a filter.
fn backup_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = vec![];
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for entry in entries {
            let p = entry?.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|e| e == "m3u8" || e == "json") {
                files.push(p);
            }
        }
    }
    let smart: HashSet<PathBuf> = files
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .filter(|p| {
            std::fs::read(p)
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .is_some_and(|v| v["smart"] == true)
        })
        .map(|p| p.with_extension(""))
        .collect();
    files.retain(|p| !smart.contains(&p.with_extension("")));
    files.sort();
    Ok(files)
}

/// Write via a temp file and rename, so an interrupted run never leaves a half-written backup.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

fn new_track<'a>(recording_mbid: &'a str, r: &'a Recording) -> NewTrack<'a> {
    NewTrack {
        recording_mbid,
        title: &r.title,
        track_no: r.track_no,
        artist: &r.artist,
        album_artist_mbid: &r.album_artist_mbid,
        album_artist: &r.album_artist,
        album_mbid: &r.album_mbid,
        album: &r.album,
        year: r.year,
        artist_mbid: r.artist_mbid.as_deref(),
        duration_ms: r.length_ms,
    }
}

/// "weekly-jams" → "Weekly Jams". Stable across weeks, which is what you want as a Plex playlist name.
/// What a playlist is called on the targets. `all` is `Db::playlists()`, newest first, so the
/// first row of a kind is its current playlist and gets `sync.playlist_title`; older rows keep
/// their full ListenBrainz title, which is also what their archive file says.
pub fn remote_title(cfg: &Config, p: &Playlist, all: &[Playlist]) -> String {
    match all.iter().find(|q| q.kind == p.kind) {
        Some(cur) if cur.id == p.id => {
            cfg.sync.playlist_title.replace("{name}", &display_name(&p.kind)).replace("{kind}", &p.kind)
        }
        _ => p.name.clone(),
    }
}

/// First `YYYY-MM-DD` in a ListenBrainz title ("Weekly Jams for rob, week of 2026-10-05 Mon").
fn date_in(title: &str) -> Option<String> {
    let b = title.as_bytes();
    (0..b.len().saturating_sub(9))
        .map(|i| &title[i..i + 10])
        .find(|w| w.bytes().enumerate().all(|(i, c)| if matches!(i, 4 | 7) { c == b'-' } else { c.is_ascii_digit() }))
        .map(str::to_string)
}

fn display_name(kind: &str) -> String {
    kind.split('-')
        .map(|w| {
            let mut c = w.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

async fn info(tx: &Sender<Event>, msg: String) -> Result<()> {
    tx.send(Event::Info { msg }).await.context("client went away")
}

async fn progress(tx: &Sender<Event>, msg: String) -> Result<()> {
    tx.send(Event::Progress { msg }).await.context("client went away")
}

async fn data(tx: &Sender<Event>, data: Value) -> Result<()> {
    tx.send(Event::Data { data }).await.context("client went away")
}

#[cfg(test)]
mod tests {
    #[test]
    fn display_name_title_cases_kind() {
        assert_eq!(super::date_in("Weekly Jams for rob, week of 2026-10-05 Mon"), Some("2026-10-05".into()));
        assert_eq!(super::date_in("Daily Jams"), None);
        assert_eq!(super::display_name("weekly-jams"), "Weekly Jams");
        assert_eq!(super::display_name("daily-jams"), "Daily Jams");
    }
}
