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

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::mpsc::Sender;

use crate::{
    App, Event,
    app::{display_name, info, new_track, progress_at},
    cli::Group,
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
    // "Title | Artist" and "Title // Artist" put the title first: turned into "Artist - Title".
    // With a dash already there ("Artist - Title | Label") the bar is just another separator.
    let dashed = [" - ", " – ", " — "].iter().any(|d| out.contains(d));
    let out = match [" | ", " // "].iter().find_map(|sep| out.split_once(sep)) {
        Some((title, artist)) if !dashed => format!("{} - {}", artist.trim(), title.trim()),
        _ => out.replace(" | ", " - ").replace(" // ", " - "),
    };
    // Trailing "HD"-style words go.
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
        let (candidates, rescued) = self.mb.search_candidates_or_primary(&source.artist, &source.title, length).await?;
        let (sure, reason) = match rescued {
            Some(note) => (false, note),
            None => judge(&source, &candidates),
        };
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
    /// Unsure, but a fallback (primary artist, the album's tracklist) found a likely match: the
    /// top candidate, to confirm rather than look for.
    pub close: bool,
    /// The library already has the top candidate.
    pub have: bool,
}

/// A playlist read and matched, not yet imported.
#[derive(Clone, Debug)]
pub struct PlaylistPlan {
    pub id: String,
    pub name: String,
    pub entries: Vec<PlaylistEntry>,
    /// The link looks like an album (a YouTube Music album list, or named after the release its
    /// songs are on): a hint for choosing how to group it, never a decision.
    pub album: bool,
}

/// What to take for an entry that is not sure.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Pick {
    /// That candidate.
    Candidate(usize),
    /// The video's own audio, tagged from the playlist: for music MusicBrainz doesn't know
    /// (game soundtracks, small uploads).
    AsIs,
}

impl PlaylistPlan {
    pub fn unsure(&self) -> impl Iterator<Item = &PlaylistEntry> {
        self.entries.iter().filter(|e| !e.sure)
    }
}

/// How an import went: tracks taken, downloads, the failures by name, and how they were grouped.
#[derive(Debug, Default)]
pub struct ImportReport {
    pub tracks: usize,
    pub downloaded: usize,
    pub failed: Vec<String>,
    pub group: Group,
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
            // A video imported before keeps its track: MusicBrainz's search ranks ties in no stable
            // order, and a second run must not pick another recording of the same song.
            let earlier = match &source.video_id {
                Some(v) => {
                    let tid = self.db().meta_get(&format!("yt.{id}.{v}"))?.and_then(|s| s.parse::<i64>().ok());
                    match tid {
                        Some(tid) => self.db().track_by_id(tid)?,
                        None => None,
                    }
                }
                None => None,
            };
            if let Some(t) = earlier {
                let c = Candidate {
                    mbid: t.recording_mbid.clone(),
                    title: t.title.clone(),
                    artist: t.artist.clone(),
                    album: t.album.clone(),
                    length_ms: t.duration_ms,
                };
                let have = present(&t);
                entries.push(PlaylistEntry {
                    pos,
                    source,
                    candidates: vec![c],
                    sure: true,
                    reason: "imported before".into(),
                    close: false,
                    have,
                });
                continue;
            }
            let length = if source.music { source.duration_ms } else { None };
            let (mut candidates, rescued) =
                match self.mb.search_candidates_or_primary(&source.artist, &source.title, length).await {
                    Ok(c) => c,
                    Err(e) => {
                        info(tx, format!("{pos}: musicbrainz: {e:#}")).await?;
                        (vec![], None)
                    }
                };
            prefer_named_release(&mut candidates, &name);
            let close = rescued.is_some();
            let (sure, reason) = match rescued {
                Some(note) => (false, note),
                None => judge(&source, &candidates),
            };
            let have = match candidates.first() {
                Some(c) => {
                    let t = self.db().track_by_mbid(&c.mbid)?;
                    t.filter(present).is_some()
                }
                None => false,
            };
            entries.push(PlaylistEntry { pos, source, candidates, sure, reason, close, have });
        }
        // Songs taken as is earlier carry the playlist's own name as album: no evidence.
        let named = entries.iter().filter(|e| {
            e.sure && e.candidates.first().is_some_and(|c| library::is_uuid(&c.mbid) && named_in(&c.album, &name))
        });
        let (named, sure) = (named.count(), entries.iter().filter(|e| e.sure).count());
        let album = id.starts_with("OLAK5uy_") || (named > 0 && named * 2 >= sure);
        let mut plan = PlaylistPlan { id, name, entries, album };
        // An album's playlist: what the search missed (a typo, an extra guest, a longer title)
        // is looked up in the album's own tracklist by position.
        if plan.unsure().next().is_some()
            && let Some(tracks) = self.named_release_tracks(&plan, tx).await?
        {
            for pos in album_fallback(&mut plan, &tracks) {
                let e = &mut plan.entries[pos];
                let t = self.db().track_by_mbid(&e.candidates[0].mbid)?;
                e.have = t.filter(present).is_some();
            }
        }
        Ok(plan)
    }

    /// The tracklist of the release a playlist is named after, found through a sure entry whose
    /// recording sits on it. `None` when no entry leads there.
    async fn named_release_tracks(&self, plan: &PlaylistPlan, tx: &Sender<Event>) -> Result<Option<Vec<Candidate>>> {
        let lead = plan
            .entries
            .iter()
            .filter(|e| e.sure)
            .filter_map(|e| e.candidates.first())
            .find(|c| named_in(&c.album, &plan.name));
        let Some(c) = lead else { return Ok(None) };
        let r = match self.mb.recording(&c.mbid).await {
            Ok(r) => r,
            Err(e) => {
                info(tx, format!("album lookup: {e:#}")).await?;
                return Ok(None);
            }
        };
        if !named_in(&r.album, &plan.name) {
            return Ok(None);
        }
        match self.mb.release_tracks(&r.album_mbid).await {
            Ok(t) => Ok(Some(t)),
            Err(e) => {
                info(tx, format!("album lookup: {e:#}")).await?;
                Ok(None)
            }
        }
    }

    /// Import a plan: the sure entries' top match plus `picks` for unsure ones; download what is
    /// missing. The songs always come into the library; `group` says whether they also form a
    /// playlist (YouTube's order, written and pushed). Imported as a playlist before and grouped
    /// otherwise now, that playlist goes (backed up first) and the songs stay.
    pub(crate) async fn apply_playlist(
        &self,
        plan: &PlaylistPlan,
        picks: &[(usize, Pick)],
        group: Group,
        tx: &Sender<Event>,
    ) -> Result<ImportReport> {
        // Taken as is (now, or on an earlier run: the remembered track is a video), the songs
        // share one album when grouped as one; its artist is theirs when they agree.
        let as_is_entry = |e: &PlaylistEntry| {
            (!e.sure && picks.contains(&(e.pos, Pick::AsIs)))
                || (e.sure && e.candidates.first().is_some_and(|c| c.mbid.starts_with("youtube:")))
        };
        let as_is: Vec<&PlaylistEntry> = plan.entries.iter().filter(|e| as_is_entry(e)).collect();
        let album_artist = match as_is.first().map(|e| e.source.artist.as_str()) {
            Some(a) if !a.is_empty() && as_is.iter().all(|e| e.source.artist == a) => a.to_string(),
            _ => "Various Artists".to_string(),
        };
        let mut ids = vec![];
        let mut taken: Vec<(String, i64)> = vec![];
        let mut moves: HashMap<PathBuf, PathBuf> = HashMap::new();
        for e in &plan.entries {
            let pick = if e.sure {
                Some(Pick::Candidate(0))
            } else {
                picks.iter().find(|(p, _)| *p == e.pos).map(|(_, k)| *k)
            };
            let tid = match pick {
                None => continue,
                Some(Pick::AsIs) => {
                    let (Some(v), Some(url)) = (&e.source.video_id, &e.source.url) else { continue };
                    let mbid = format!("youtube:{v}");
                    let existing = self.db().track_by_mbid(&mbid)?;
                    match existing {
                        Some(t) => t.id,
                        None => {
                            let r = as_is_recording(e, plan, &album_artist, group);
                            let id = self.db().insert_track(&new_track(&mbid, &r))?;
                            self.db().set_source_url(id, url)?;
                            id
                        }
                    }
                }
                Some(Pick::Candidate(ci)) => {
                    let Some(c) = e.candidates.get(ci) else { continue };
                    // Bind first: the DB guard must not live across the await below.
                    let existing = self.db().track_by_mbid(&c.mbid)?;
                    match existing {
                        Some(t) => t.id,
                        None => match self.mb.recording(&c.mbid).await {
                            Ok(r) => self.db().insert_track(&new_track(&c.mbid, &r))?,
                            Err(err) => {
                                info(tx, format!("{}: {} - {}: {err:#}", e.pos, c.artist, c.title)).await?;
                                continue;
                            }
                        },
                    }
                }
            };
            // A song taken as is before follows the grouping chosen now.
            if as_is_entry(e) {
                let want = as_is_recording(e, plan, &album_artist, group);
                if let Err(err) = self.regroup(tid, &want, group != Group::Album, &mut moves, tx).await {
                    info(tx, format!("{}: {} - regrouping: {err:#}", e.pos, e.source.title)).await?;
                }
            }
            if !ids.contains(&tid) {
                ids.push(tid);
            }
            if let Some(v) = &e.source.video_id {
                taken.push((format!("yt.{}.{v}", plan.id), tid));
            }
        }
        if !moves.is_empty() {
            self.rewrite_m3us(None)?;
            self.follow_moves(&moves, tx).await?;
        }
        for (key, tid) in &taken {
            self.db().meta_set(key, &tid.to_string())?;
        }
        let kind = format!("youtube-{}", plan.id);
        let earlier = self.db().find_playlist(&kind)?;
        let old = match &earlier {
            Some(p) => self.db().playlist_tracks(p.id)?,
            None => vec![],
        };
        let pid = match (group, &earlier) {
            (Group::Album | Group::None, Some(p)) => {
                info(tx, format!("{}: no playlist this time; the earlier one goes, the songs stay", plan.name)).await?;
                self.drop_playlist(p, tx).await?;
                None
            }
            (Group::Album | Group::None, None) => None,
            (Group::Playlist, _) => {
                let pid = self.db().upsert_playlist(&kind, &plan.name, &kind, &chrono_now()[..10])?;
                self.db().set_playlist_tracks(pid, &ids)?;
                Some(pid)
            }
        };
        // A row an earlier run of this import added and that is now off it goes again when
        // nothing was downloaded for it and no other playlist wants it.
        for t in old.iter().filter(|t| !ids.contains(&t.id) && t.status == "wanted") {
            let elsewhere = self.db().playlists_with_track(t.id)?;
            if elsewhere.is_empty() {
                self.db().delete_track(t.id)?;
                info(tx, format!("dropped {} - {}: no longer on {}", t.artist, t.title, plan.name)).await?;
            }
        }
        info(tx, format!("{}: {} tracks; downloading what is missing", plan.name, ids.len())).await?;
        let mut before = vec![];
        for id in &ids {
            let t = self.db().track_by_id(*id)?;
            if t.is_some_and(|t| !present(&t)) {
                before.push(*id);
            }
        }
        // The usual download step (external libraries first, lyrics after), for these tracks only.
        let downloaded = self.sync(None, true, true, Some(&ids), tx).await;
        if let Some(pid) = pid {
            self.publish_playlist(pid, tx).await?;
        }
        let mut report = ImportReport { tracks: ids.len(), group, ..Default::default() };
        for id in &before {
            let t = self.db().track_by_id(*id)?;
            match t {
                Some(t) if present(&t) => report.downloaded += 1,
                Some(t) => report.failed.push(format!("{} - {}", t.artist, t.title)),
                None => {}
            }
        }
        // Failures elsewhere in the queue are not this import's; report ours, not the sync's.
        if let Err(e) = downloaded
            && report.failed.is_empty()
        {
            tracing::debug!("import: other downloads failed: {e:#}");
        }
        Ok(report)
    }

    /// Give a track taken as is the tags `want` says (another grouping than before): update the
    /// row, move the file where the template now puts it, retag it. A single's folder gets the
    /// song's own picture as cover.jpg; an album's folder gets one when it has none.
    async fn regroup(
        &self,
        tid: i64,
        want: &Recording,
        single: bool,
        moves: &mut HashMap<PathBuf, PathBuf>,
        tx: &Sender<Event>,
    ) -> Result<()> {
        let Some(t) = self.db().track_by_id(tid)? else { return Ok(()) };
        if t.album == want.album && t.album_artist == want.album_artist && t.track_no == want.track_no {
            return Ok(());
        }
        self.db().insert_track(&new_track(&t.recording_mbid, want))?;
        // Not downloaded yet: the download tags and files it the new way.
        let (Some(old_rel), "ready") = (&t.path, t.status.as_str()) else { return Ok(()) };
        let cfg = &self.config;
        let fresh = self.db().track_by_id(tid)?.context("track vanished")?;
        let ext = Path::new(old_rel).extension().and_then(|e| e.to_str()).unwrap_or("bin");
        let mut rel = PathBuf::from(old_rel);
        let new_rel = library::render_template(&cfg.path_template, &fresh, ext)?;
        if new_rel != rel {
            let new_rel = library::unique(&cfg.library, &new_rel);
            library::move_track(&cfg.library, &rel, &new_rel)?;
            // A single's folder is tapedeck's own (one song and the cover.jpg written for it):
            // once the song has left, the cover goes too and the folder with it.
            let was_single = crate::covers::youtube_id(&t).is_some_and(|v| t.album_mbid == format!("youtube:{v}"));
            if let Some(old_dir) = cfg.library.join(&rel).parent()
                && was_single
                && old_dir.is_dir()
                && library::walk(old_dir).is_empty()
            {
                for name in crate::steam::COVER_NAMES {
                    let _ = std::fs::remove_file(old_dir.join(name));
                }
                let _ = std::fs::remove_dir(old_dir);
            }
            self.db().set_path(tid, &new_rel.to_string_lossy())?;
            moves.insert(cfg.library.join(&rel), cfg.library.join(&new_rel));
            info(tx, format!("  {} → {}", rel.display(), new_rel.display())).await?;
            rel = new_rel;
        }
        let file = cfg.library.join(&rel);
        library::tag(&file, &fresh)?;
        if let (Some(dir), Some(pic)) = (file.parent(), crate::steam::cover_for(&file)) {
            let cover = dir.join("cover.jpg");
            if single || !crate::steam::COVER_NAMES.iter().any(|n| dir.join(n).is_file()) {
                std::fs::write(&cover, pic).with_context(|| format!("writing {}", cover.display()))?;
            }
        }
        Ok(())
    }

    /// `td import <link>`: the dry run, or with `apply` the import. `also` takes the top match
    /// of those unsure positions, `as_is` the video itself for those (all unmatched with
    /// `as_is_all`); `group` is how the songs are grouped.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn import_playlist(
        &self,
        url: &str,
        apply: bool,
        also: &[usize],
        as_is: &[usize],
        as_is_all: bool,
        group: Group,
        tx: &Sender<Event>,
    ) -> Result<()> {
        let plan = self.analyze_playlist(url, tx).await?;
        let mut picks: Vec<(usize, Pick)> = vec![];
        for e in plan.unsure() {
            if also.contains(&e.pos) && !e.candidates.is_empty() {
                picks.push((e.pos, Pick::Candidate(0)));
            } else if as_is.contains(&e.pos) || (as_is_all && e.candidates.is_empty()) {
                picks.push((e.pos, Pick::AsIs));
            }
        }
        let rows: Vec<Value> = plan
            .entries
            .iter()
            .map(|e| {
                let pick = picks.iter().find(|(p, _)| *p == e.pos).map(|(_, k)| *k);
                let status = match (e.sure, pick, e.have) {
                    (_, Some(Pick::AsIs), _) => "as is",
                    (true, _, true) | (false, Some(_), true) => "have",
                    (true, ..) => "new",
                    (false, Some(_), false) => "take",
                    (false, None, _) => "ask",
                };
                let best = e.candidates.first().filter(|_| pick != Some(Pick::AsIs));
                json!({
                    "pick": e.pos,
                    "status": status,
                    "artist": best.map_or(e.source.artist.as_str(), |c| c.artist.as_str()),
                    "title": best.map_or(e.source.title.as_str(), |c| c.title.as_str()),
                    "album": best.map_or(plan.name.as_str(), |c| c.album.as_str()),
                    "note": if status == "ask" { format!("{}; sent {:?}", e.reason, e.source.raw_title) } else { String::new() },
                })
            })
            .collect();
        tx.send(Event::Data { data: json!(rows) }).await.context("client went away")?;
        let asks = rows.iter().filter(|r| r["status"] == "ask").count();
        let unmatched = plan.unsure().filter(|e| e.candidates.is_empty()).count();
        if !apply {
            let shape = match group {
                Group::Playlist => format!("as the playlist {:?}", plan.name),
                Group::Album => "as an album, no playlist".to_string(),
                Group::None => "ungrouped, no playlist".to_string(),
            };
            let hint = if plan.album && group == Group::Playlist { ", it looks like an album" } else { "" };
            let shape = format!("{shape} (--group playlist|album|none{hint})");
            let mut msg = format!("{} to take, {asks} unsure, {shape}; --apply imports", rows.len() - asks);
            if asks > 0 {
                msg.push_str(", --also N,M takes the top match of unsure ones");
            }
            if unmatched > 0 {
                msg.push_str(&format!(
                    ", --as-is N,M (or --as-is-all for the {unmatched} without a match) takes the video itself"
                ));
            }
            return info(tx, msg).await;
        }
        let r = self.apply_playlist(&plan, &picks, group, tx).await?;
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

/// The release title as whole words of the playlist title ("Glava" in "Album S.A.R.S. - Glava (2019)").
fn named_in(album: &str, playlist: &str) -> bool {
    let a = norm(album);
    !a.is_empty() && format!(" {} ", norm(playlist)).contains(&format!(" {a} "))
}

/// Close enough to be the same title: equal, one the other's start ("U se, na se, poda se" /
/// "U Se Na Se"), or a typo apart ("Razguliana" / "Razgulijana").
fn similar_title(a: &str, b: &str) -> bool {
    let (a, b) = (norm(&strip_feat(a)), norm(&strip_feat(b)));
    let short = a.len().min(b.len());
    a == b
        || (short >= 4 && (a.starts_with(&b) || b.starts_with(&a)))
        || edits(&a, &b) <= (a.len().max(b.len()) / 8).max(1)
}

/// Levenshtein distance over chars.
fn edits(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = (prev + usize::from(ca != *cb)).min(row[j] + 1).min(cur + 1);
            prev = cur;
        }
    }
    row[b.len()]
}

/// Unsure entries against the album's tracklist at the same position: when the title is similar
/// or the length fits, the track becomes the first candidate and the entry a close match, asked
/// about rather than taken. Returns the indexes of the entries it touched.
fn album_fallback(plan: &mut PlaylistPlan, tracks: &[Candidate]) -> Vec<usize> {
    let mut touched = vec![];
    for (i, e) in plan.entries.iter_mut().enumerate().filter(|(_, e)| !e.sure) {
        let Some(t) = e.pos.checked_sub(1).and_then(|p| tracks.get(p)) else { continue };
        let title_ok = similar_title(&e.source.title, &t.title);
        let len_ok = match (e.source.duration_ms, t.length_ms) {
            (Some(a), Some(b)) => {
                ((a - b).abs() as f64) <= crate::download::ytdlp::max_drift(b as f64 / 1000.0) * 1000.0
            }
            _ => false,
        };
        if !(title_ok || len_ok) {
            continue;
        }
        let secs = |ms: Option<i64>| ms.map_or("?".to_string(), |ms| format!("{}s", ms / 1000));
        let lens = format!("{} vs {}", secs(e.source.duration_ms), secs(t.length_ms));
        let why = match (title_ok, len_ok) {
            (true, true) => format!("YouTube {:?} ≈ {:?}", e.source.title, t.title),
            (true, false) => format!("similar title {:?}, but the length is off", t.title),
            _ => format!("the length fits, but it is called {:?}", t.title),
        };
        e.reason = format!("close match: {why} ({} #{}, {lens})", t.album, e.pos);
        e.close = true;
        e.candidates.retain(|c| c.mbid != t.mbid);
        e.candidates.insert(0, t.clone());
        touched.push(i);
    }
    touched
}

/// For an album's playlist ("Album - Glava", "S.A.R.S. - Glava (2019)"): among candidates with the
/// top one's title, those from a release named in the playlist title go first. A best-of carries
/// the same songs as other recordings.
fn prefer_named_release(cands: &mut [Candidate], playlist: &str) {
    let Some(top) = cands.first().map(|c| norm(&c.title)) else { return };
    let named = |c: &Candidate| norm(&c.title) == top && named_in(&c.album, playlist);
    // Stable: the search's order holds within each group.
    cands.sort_by_key(|c| !named(c));
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

/// A playlist entry taken as is, tagged from the video: grouped as an album, track N of an album
/// named after the playlist (no year: one folder for all of it); otherwise a single of its own,
/// so every song keeps its own cover (Plex shows one picture per album).
fn as_is_recording(e: &PlaylistEntry, plan: &PlaylistPlan, album_artist: &str, group: Group) -> Recording {
    let artist = if e.source.artist.is_empty() { album_artist.to_string() } else { e.source.artist.clone() };
    if group != Group::Album {
        return Recording {
            title: e.source.title.clone(),
            artist: artist.clone(),
            artist_mbid: None,
            album_artist_mbid: format!("youtube:{}", norm(&artist).replace(' ', "-")),
            album_artist: artist,
            album: e.source.title.clone(),
            album_mbid: format!("youtube:{}", e.source.video_id.clone().unwrap_or_default()),
            year: e.source.year,
            track_no: Some(1),
            length_ms: e.source.duration_ms,
        };
    }
    Recording {
        title: e.source.title.clone(),
        artist,
        artist_mbid: None,
        album_artist: album_artist.to_string(),
        album_artist_mbid: format!("youtube:{}", norm(album_artist).replace(' ', "-")),
        album: plan.name.clone(),
        album_mbid: format!("youtube:{}", plan.id),
        year: None,
        track_no: Some(e.pos as i64),
        length_ms: e.source.duration_ms,
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
    fn album_tracklist_catches_what_the_search_missed() {
        let track = |mbid: &str, title: &str, len: i64| Candidate {
            mbid: mbid.into(),
            title: title.into(),
            artist: "S.A.R.S.".into(),
            album: "Poslednji Album".into(),
            length_ms: Some(len),
        };
        let tracks = vec![
            track("t1", "Prva", 180_000),
            track("t2", "Glupost", 202_000),
            track("t3", "Treća", 190_000),
            track("t4", "U Se Na Se", 173_000),
            track("t5", "Peta", 200_000),
        ];
        let entry = |pos: usize, title: &str, secs: i64| PlaylistEntry {
            pos,
            source: Source { title: title.into(), duration_ms: Some(secs * 1000), ..Default::default() },
            candidates: vec![],
            sure: false,
            reason: "no MusicBrainz match".into(),
            close: false,
            have: false,
        };
        let mut plan = PlaylistPlan {
            id: "PL".into(),
            name: "Album S.A.R.S. - \"Poslednji album\" (2016)".into(),
            album: true,
            entries: vec![
                entry(2, "Glupost", 203),
                entry(4, "U se, na se, poda se (feat Kandžija)", 174),
                entry(5, "Nešto drugo", 320),
            ],
        };
        assert_eq!(album_fallback(&mut plan, &tracks), vec![0, 1]);
        // Found, but asked about: never taken without a yes.
        assert!(!plan.entries[0].sure && plan.entries[0].close && plan.entries[0].candidates[0].mbid == "t2");
        assert!(!plan.entries[1].sure && plan.entries[1].close && plan.entries[1].candidates[0].mbid == "t4");
        assert!(plan.entries[1].reason.starts_with("close match: YouTube"), "{}", plan.entries[1].reason);
        assert!(plan.entries[1].reason.contains("(Poslednji Album #4, 174s vs 173s)"), "{}", plan.entries[1].reason);
        assert!(!plan.entries[2].close && plan.entries[2].candidates.is_empty(), "neither title nor length fits");
        assert!(similar_title("Razguliana", "Razgulijana"));
        assert!(!similar_title("Lutka", "Glava"));
        assert!(named_in("Poslednji Album", &plan.name));
    }

    #[test]
    fn taken_as_is_tags_follow_the_grouping() {
        let entry = |pos: usize, title: &str, sure: bool| PlaylistEntry {
            pos,
            source: Source {
                title: title.into(),
                artist: "One More Lawn (Radio)".into(),
                video_id: Some(format!("v{pos}")),
                duration_ms: Some(157_000),
                year: Some(2025),
                ..Default::default()
            },
            candidates: vec![],
            sure,
            reason: "no MusicBrainz match".into(),
            close: false,
            have: false,
        };
        let plan = PlaylistPlan {
            id: "PLJ19oo9w4X5A".into(),
            name: "One More Lawn - Full Radio (All Songs)".into(),
            entries: vec![entry(1, "Me Pica El Ano", false), entry(2, "Wrong Pill", false), entry(3, "Hit", true)],
            album: false,
        };
        let r = as_is_recording(&plan.entries[1], &plan, "One More Lawn (Radio)", Group::Album);
        assert_eq!((r.album.as_str(), r.track_no, r.year), ("One More Lawn - Full Radio (All Songs)", Some(2), None));
        assert_eq!(
            (r.title.as_str(), r.artist.as_str(), r.album_mbid.as_str()),
            ("Wrong Pill", "One More Lawn (Radio)", "youtube:PLJ19oo9w4X5A")
        );
        // As a playlist or ungrouped: a single of its own, so it keeps its own cover in Plex.
        for group in [Group::Playlist, Group::None] {
            let single = as_is_recording(&plan.entries[1], &plan, "One More Lawn (Radio)", group);
            assert_eq!(
                (single.album.as_str(), single.album_mbid.as_str(), single.track_no),
                ("Wrong Pill", "youtube:v2", Some(1))
            );
        }
    }

    #[test]
    fn album_named_in_the_playlist_title_wins_ties() {
        let c = |mbid: &str, title: &str, album: &str| Candidate {
            mbid: mbid.into(),
            title: title.into(),
            artist: "S.A.R.S.".into(),
            album: album.into(),
            length_ms: None,
        };
        let mut v = vec![c("best-of", "Teško je", "10"), c("album", "Teško Je", "Glava"), c("other", "Lutka", "Glava")];
        prefer_named_release(&mut v, "Album S.A.R.S. - Glava (2019)");
        assert_eq!(v.iter().map(|c| c.mbid.as_str()).collect::<Vec<_>>(), ["album", "best-of", "other"]);
        // "10" is not a word of the title even though "2010" holds the digits.
        let mut w = vec![c("a", "X", "Glava"), c("b", "X", "10")];
        prefer_named_release(&mut w, "Something 2010");
        assert_eq!(w[0].mbid, "a");
    }

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
        // A game radio's "Song | Station": the song is the title.
        let clean = clean_title("Me Pica El Ano | One More Lawn (Radio)");
        assert_eq!(
            split_artist_title(&clean, Some("Superdou")),
            ("One More Lawn (Radio)".into(), "Me Pica El Ano".into())
        );
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
