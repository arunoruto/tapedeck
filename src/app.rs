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
    Info { msg: String },
    Data { data: Value },
    Error { msg: String },
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
                data(tx, json!({ "tracks": counts, "playlists": playlists })).await
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
            Command::Push { playlist, to } => {
                let p = self.db().find_playlist(&playlist)?.with_context(|| format!("no playlist {playlist:?}"))?;
                self.push(&p, to.as_deref(), tx).await
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
                    library::remove_track(&self.config.library, Path::new(rel))?;
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
            Command::Import { dir, apply, delete_duplicates } => self.import(&dir, apply, delete_duplicates, tx).await,
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
            info(tx, format!("checking {}/{total}: {} - {}", i + 1, t.artist, t.title)).await?;
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
    async fn import(&self, dir: &Path, apply: bool, delete_duplicates: bool, tx: &Sender<Event>) -> Result<()> {
        let cfg = &self.config;
        let found = library::scan(dir)?;
        info(tx, format!("{} audio files under {}", found.len(), dir.display())).await?;
        let (mut unresolved, mut lookup_failed) = (0, 0);

        // Pass 1: every file to a recording MBID, grouped in first-seen order. Explo drops the
        // same track into every playlist folder it appears in, so groups are the norm.
        let mut groups: Vec<(String, Vec<&library::Found>)> = vec![];
        let mut index: HashMap<String, usize> = HashMap::new();
        // Files the search could not place; `None` is no match, `Some` a failed lookup.
        let mut pending: Vec<(&library::Found, Option<anyhow::Error>)> = vec![];
        for f in &found {
            let mbid = match &f.recording_mbid {
                Some(m) => m.clone(),
                None => match self.mb.search_recording(&f.artist, &f.title, f.duration_ms).await {
                    Ok(Some(m)) => m,
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
        let (mut imported, mut duplicates) = (0, 0);
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
                    info(tx, format!("import {} → {} - {} [{}]", best.path.display(), r.artist, r.title, r.album))
                        .await?;
                    imported += 1;
                    let mut dest = None;
                    if apply {
                        let id = self.db().insert_track(&new_track(&mbid, &r))?;
                        let t = self.db().track_by_mbid(&mbid)?.context("track vanished")?;
                        let rel = library::file_track(&cfg.library, &cfg.path_template, &t, &best.path)?;
                        self.db().mark_ready(id, &rel.to_string_lossy(), None)?;
                        let d = cfg.library.join(rel);
                        moved.insert(best.path.clone(), d.clone());
                        dest = Some(d);
                    }
                    (Some(&best.path), format!("{} - {}", r.artist, r.title), dest)
                }
            };
            for f in files.iter().filter(|f| Some(&f.path) != kept) {
                duplicates += 1;
                if apply && delete_duplicates {
                    std::fs::remove_file(&f.path)?;
                    info(tx, format!("deleted duplicate of {label}: {}", f.path.display())).await?;
                    if let Some(d) = &dest {
                        moved.insert(f.path.clone(), d.clone());
                    }
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
        let mode = if apply { "" } else { " (dry run; pass --apply to move files)" };
        info(tx, format!("{imported} imported, {duplicates} duplicates, {unresolved} no match, {lookup_failed} lookups failed{mode}")).await?;
        ensure!(
            lookup_failed == 0,
            "{lookup_failed} MusicBrainz lookups failed; run import again later for those files"
        );
        Ok(())
    }

    /// Write a superseded playlist to `archive/<kind>/<period>.m3u8` and drop its old files.
    fn archive(&self, p: &Playlist) -> Result<PathBuf> {
        let cfg = &self.config;
        // The title's date ("week of 2026-10-05", "2026-10-17 Sat") is what the user knows the
        // playlist by; ListenBrainz's own date field can be the day before.
        let date = date_in(&p.name)
            .or_else(|| p.date.clone().filter(|d| !d.is_empty()))
            .unwrap_or_else(|| p.fetched_at.get(..10).unwrap_or("undated").to_string());
        let dir = cfg.playlist_dir().join("archive").join(&p.kind);
        let tracks = self.db().playlist_tracks(p.id)?;
        let file = m3u::write(&dir, &cfg.library, &m3u::period(&p.kind, &date), &p.name, &tracks)?;
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
    fn archive_legacy(&self, kind: &str, current: &Path) -> Result<()> {
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
            let target = archive.join(format!("{}.m3u8", m3u::period(kind, &date)));
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
                let names: Vec<String> = std::fs::read_dir(t.path())?
                    .flatten()
                    .map(|f| f.file_name().to_string_lossy().into_owned())
                    .collect();
                for name in &names {
                    let Some((stem, ext)) = name.rsplit_once('.') else { continue };
                    let slugged = format!("{}.{ext}", m3u::slug(stem));
                    // Literal comparison again: a case-insensitive filesystem says the slug "exists".
                    if slugged != *name && !names.contains(&slugged) {
                        std::fs::rename(t.path().join(name), t.path().join(&slugged))?;
                    }
                }
            }
        }
        let mut kinds: Vec<String> = self.db().playlists()?.into_iter().map(|p| p.kind).collect();
        kinds.extend(cfg.listenbrainz.playlists.iter().cloned());
        kinds.sort();
        kinds.dedup();
        for kind in kinds {
            self.archive_legacy(&kind, &dir.join(format!("{kind}.m3u8")))?;
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
                let rel = library::file_track(&cfg.library, &cfg.path_template, t, &got.path)?;
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
            for old in rows {
                let archived = self.archive(old)?;
                self.db().set_playlist_m3u(old.id, &archived.to_string_lossy())?;
            }
            let tracks = self.db().playlist_tracks(cur.id)?;
            let file = m3u::write(&cfg.playlist_dir(), &cfg.library, kind, &cur.name, &tracks)?;
            self.remove_stale(cur, &file);
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
        let title = self.title(&p.kind);
        for target in self.targets.iter().filter(|t| only.is_none_or(|o| o == t.name())) {
            match target.sync_playlist(&title, &files).await {
                Ok(remote) => {
                    self.db().record_sync(p.id, target.name(), &remote)?;
                    info(tx, format!("{}: pushed {title} ({} tracks)", target.name(), files.len())).await?;
                }
                Err(e) => info(tx, format!("{}: push of {title} failed: {e:#}", target.name())).await?,
            }
        }
        Ok(())
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
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for entry in entries {
            let p = entry?.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            match p.extension().and_then(|e| e.to_str()) {
                Some("m3u8") => {
                    let body = std::fs::read_to_string(&p)?;
                    let new: String = body
                        .lines()
                        .map(|l| moved.get(Path::new(l)).map(|n| n.to_string_lossy().into_owned()).unwrap_or(l.into()))
                        .map(|l| l + "\n")
                        .collect();
                    if new != body {
                        std::fs::write(&p, new)?;
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
                        std::fs::write(&p, serde_json::to_vec_pretty(&v)?)?;
                        changed += 1;
                    }
                }
                _ => {}
            }
        }
    }
    Ok(changed)
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
