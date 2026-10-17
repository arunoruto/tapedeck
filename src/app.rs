//! The one place commands are executed, whether invoked locally or through the daemon.

use std::{path::PathBuf, sync::Mutex};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc::Sender;

use crate::{
    cli::Command,
    config::Config,
    db::{Db, NewTrack, Playlist},
    download::{self, Downloader},
    library, listenbrainz, m3u,
    musicbrainz::MusicBrainz,
    sync::{self, SyncTarget},
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
            Command::Daemon => bail!("`daemon` runs in the foreground; it cannot be sent to a running daemon"),
            Command::Sync { playlist, no_push } => self.sync(playlist, no_push, tx).await,
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
            Command::Retry => {
                let n = self.db().retry_failed()?;
                info(tx, format!("{n} tracks reset to wanted")).await
            }
            Command::Source { track, url } => {
                ensure!(self.db().set_source_url(track, &url)?, "no track with id {track}");
                info(tx, format!("track {track} queued for download from {url}")).await
            }
        }
    }

    async fn sync(&self, only: Option<String>, no_push: bool, tx: &Sender<Event>) -> Result<()> {
        let cfg = &self.config;

        // 1. ListenBrainz playlists → DB rows (new tracks resolved through MusicBrainz).
        let mut fetched: Vec<(i64, String)> = vec![];
        for lb in listenbrainz::newest_per_kind(self.lb.created_for(&cfg.listenbrainz.user).await?) {
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
                        self.db().insert_track(&NewTrack {
                            recording_mbid: &t.recording_mbid,
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
                        })?
                    }
                };
                ids.push(id);
            }
            let pid = self.db().upsert_playlist(&lb.mbid, &lb.title, &lb.kind)?;
            self.db().set_playlist_tracks(pid, &ids)?;
            // m3u8 file name: "Weekly Jams 2026-10-05", short and sortable; the LB title stays inside the file.
            fetched.push((pid, format!("{} {}", display_name(&lb.kind), lb.date.get(..10).unwrap_or("undated"))));
        }

        // 2. Download whatever is still wanted, tag it, file it.
        let wanted = self.db().wanted_tracks()?;
        info(tx, format!("{} tracks to download", wanted.len())).await?;
        let (mut done, mut failed) = (0, 0);
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
                }
                Err(e) => {
                    self.db().mark_failed(t.id, &format!("{e:#}"))?;
                    info(tx, format!("failed {} - {}: {e:#}", t.artist, t.title)).await?;
                    failed += 1;
                }
            }
        }

        // 3. m3u per playlist, then push.
        let playlists = self.db().playlists()?;
        for (pid, stem) in &fetched {
            let Some(p) = playlists.iter().find(|p| p.id == *pid) else { continue };
            let tracks = self.db().playlist_tracks(p.id)?;
            let file = m3u::write(&cfg.playlist_dir(), &cfg.library, stem, &p.name, &tracks)?;
            self.db().set_playlist_m3u(p.id, &file.to_string_lossy())?;
            info(tx, format!("wrote {}", file.display())).await?;
            if !no_push {
                self.push(p, None, tx).await?;
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
        let title =
            self.config.sync.playlist_title.replace("{name}", &display_name(&p.kind)).replace("{kind}", &p.kind);
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

/// "weekly-jams" → "Weekly Jams". Stable across weeks, which is what you want as a Plex playlist name.
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
        assert_eq!(super::display_name("weekly-jams"), "Weekly Jams");
        assert_eq!(super::display_name("daily-jams"), "Daily Jams");
    }
}
