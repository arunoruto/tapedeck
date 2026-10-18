use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;

pub struct Db(Connection);

#[derive(Serialize, Clone, Debug)]
pub struct Artist {
    pub id: i64,
    pub mbid: String,
    /// The name MusicBrainz currently gives the artist entity.
    pub name: String,
    /// A folder name pinned by the user, used instead of `name` when set.
    pub folder: Option<String>,
    pub tracks: i64,
}

/// A file in a read-only external library.
#[derive(Serialize, Clone, Debug)]
pub struct ExternalFile {
    pub path: String,
    pub root: String,
    pub size: i64,
    pub mtime: i64,
    pub recording_mbid: Option<String>,
    pub artist: String,
    pub title: String,
    pub duration_ms: Option<i64>,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct Track {
    pub id: i64,
    pub recording_mbid: String,
    pub title: String,
    pub artist: String,
    pub album_artist: String,
    pub album: String,
    pub year: Option<i64>,
    pub track_no: Option<i64>,
    /// Relative to the library root.
    pub path: Option<String>,
    pub status: String,
    pub source_url: Option<String>,
    pub error: Option<String>,
    pub artist_mbid: Option<String>,
    pub album_mbid: String,
    pub album_artist_mbid: String,
    pub duration_ms: Option<i64>,
}

#[derive(Serialize, Clone, Debug)]
pub struct Playlist {
    pub id: i64,
    pub lb_mbid: String,
    pub name: String,
    pub kind: String,
    pub m3u_path: Option<String>,
    /// The ListenBrainz playlist date (YYYY-MM-DD); rows from before the column exists have none.
    pub date: Option<String>,
    pub fetched_at: String,
    pub track_count: i64,
    pub synced: Vec<SyncRecord>,
}

#[derive(Serialize, Clone, Debug)]
pub struct SyncRecord {
    pub target: String,
    pub remote_id: Option<String>,
    pub synced_at: String,
}

/// Everything we know about a track in one row, so no per-field lookups later.
pub struct NewTrack<'a> {
    pub recording_mbid: &'a str,
    pub title: &'a str,
    pub track_no: Option<i64>,
    pub artist: &'a str,
    pub album_artist_mbid: &'a str,
    pub album_artist: &'a str,
    pub album_mbid: &'a str,
    pub album: &'a str,
    pub year: Option<i64>,
    pub artist_mbid: Option<&'a str>,
    pub duration_ms: Option<i64>,
}

const TRACK_SELECT: &str = "
    SELECT t.id, t.recording_mbid, t.title, t.artist, COALESCE(ar.folder, ar.name), al.title, al.year, t.track_no,
           t.path, t.status, t.source_url, t.error, t.artist_mbid, al.mbid, ar.mbid, t.duration_ms
    FROM tracks t
    JOIN albums al ON al.id = t.album_id
    JOIN artists ar ON ar.id = al.artist_id";

const EXTERNAL_SELECT: &str =
    "SELECT path, root, size, mtime, recording_mbid, artist, title, duration_ms FROM external_files";

fn external_row(r: &Row) -> rusqlite::Result<ExternalFile> {
    Ok(ExternalFile {
        path: r.get(0)?,
        root: r.get(1)?,
        size: r.get(2)?,
        mtime: r.get(3)?,
        recording_mbid: r.get(4)?,
        artist: r.get(5)?,
        title: r.get(6)?,
        duration_ms: r.get(7)?,
    })
}

fn track_row(r: &Row) -> rusqlite::Result<Track> {
    Ok(Track {
        id: r.get(0)?,
        recording_mbid: r.get(1)?,
        title: r.get(2)?,
        artist: r.get(3)?,
        album_artist: r.get(4)?,
        album: r.get(5)?,
        year: r.get(6)?,
        track_no: r.get(7)?,
        path: r.get(8)?,
        status: r.get(9)?,
        source_url: r.get(10)?,
        error: r.get(11)?,
        artist_mbid: r.get(12)?,
        album_mbid: r.get(13)?,
        album_artist_mbid: r.get(14)?,
        duration_ms: r.get(15)?,
    })
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.execute_batch(include_str!("schema.sql"))?;
        // Columns added after the first release. CREATE TABLE IF NOT EXISTS leaves existing
        // databases alone, so add them here and ignore "duplicate column".
        for (table, col) in [
            ("tracks", "artist_mbid TEXT"),
            ("tracks", "duration_ms INTEGER"),
            ("playlists", "date TEXT"),
            ("artists", "folder TEXT"),
        ] {
            if let Err(e) = conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {col}"), [])
                && !e.to_string().contains("duplicate column")
            {
                return Err(e.into());
            }
        }
        Ok(Db(conn))
    }

    pub fn track_by_mbid(&self, mbid: &str) -> Result<Option<Track>> {
        Ok(self.0.query_row(&format!("{TRACK_SELECT} WHERE t.recording_mbid = ?1"), [mbid], track_row).optional()?)
    }

    pub fn insert_track(&self, t: &NewTrack) -> Result<i64> {
        let artist_id: i64 = self.0.query_row(
            "INSERT INTO artists (mbid, name) VALUES (?1, ?2)
             ON CONFLICT(mbid) DO UPDATE SET name = excluded.name RETURNING id",
            params![t.album_artist_mbid, t.album_artist],
            |r| r.get(0),
        )?;
        let album_id: i64 = self.0.query_row(
            "INSERT INTO albums (mbid, artist_id, title, year) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(mbid) DO UPDATE SET title = excluded.title, year = excluded.year RETURNING id",
            params![t.album_mbid, artist_id, t.album, t.year],
            |r| r.get(0),
        )?;
        Ok(self.0.query_row(
            "INSERT INTO tracks (recording_mbid, album_id, title, artist, track_no, artist_mbid, duration_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(recording_mbid) DO UPDATE SET title = excluded.title, artist = excluded.artist,
                 album_id = excluded.album_id, track_no = excluded.track_no,
                 artist_mbid = excluded.artist_mbid, duration_ms = excluded.duration_ms RETURNING id",
            params![t.recording_mbid, album_id, t.title, t.artist, t.track_no, t.artist_mbid, t.duration_ms],
            |r| r.get(0),
        )?)
    }

    pub fn all_tracks(&self) -> Result<Vec<Track>> {
        self.query_tracks(&format!("{TRACK_SELECT} ORDER BY t.id"), [])
    }

    pub fn set_path(&self, id: i64, rel_path: &str) -> Result<()> {
        self.0.execute("UPDATE tracks SET path = ?2 WHERE id = ?1", params![id, rel_path])?;
        Ok(())
    }

    pub fn wanted_tracks(&self) -> Result<Vec<Track>> {
        self.query_tracks(&format!("{TRACK_SELECT} WHERE t.status = 'wanted' ORDER BY t.id"), [])
    }

    pub fn search(&self, q: &str) -> Result<Vec<Track>> {
        let like = format!("%{q}%");
        self.query_tracks(
            &format!("{TRACK_SELECT} WHERE t.title LIKE ?1 OR t.artist LIKE ?1 OR ar.name LIKE ?1 OR al.title LIKE ?1 ORDER BY ar.name, al.title, t.track_no"),
            [like],
        )
    }

    pub fn mark_ready(&self, id: i64, rel_path: &str, source_url: Option<&str>) -> Result<()> {
        self.0.execute(
            "UPDATE tracks SET status = 'ready', path = ?2, source_url = COALESCE(?3, source_url), error = NULL WHERE id = ?1",
            params![id, rel_path, source_url],
        )?;
        Ok(())
    }

    /// Change which recording a row is. Fails if another row already is that recording.
    pub fn set_recording(&self, id: i64, mbid: &str) -> Result<()> {
        self.0.execute("UPDATE tracks SET recording_mbid = ?2 WHERE id = ?1", params![id, mbid])?;
        Ok(())
    }

    /// Pin a download URL and queue the track again. Returns false if the id does not exist.
    pub fn set_source_url(&self, id: i64, url: &str) -> Result<bool> {
        let n = self.0.execute(
            "UPDATE tracks SET source_url = ?2, status = 'wanted', error = NULL WHERE id = ?1",
            params![id, url],
        )?;
        Ok(n == 1)
    }

    pub fn mark_failed(&self, id: i64, error: &str) -> Result<()> {
        self.0.execute("UPDATE tracks SET status = 'failed', error = ?2 WHERE id = ?1", params![id, error])?;
        Ok(())
    }

    /// Album artists with their MusicBrainz name, pinned folder name (if any) and track count.
    pub fn artists(&self) -> Result<Vec<Artist>> {
        let mut st = self.0.prepare(
            "SELECT ar.id, ar.mbid, ar.name, ar.folder,
                    (SELECT COUNT(*) FROM tracks t JOIN albums al ON al.id = t.album_id WHERE al.artist_id = ar.id)
             FROM artists ar
             WHERE EXISTS (SELECT 1 FROM tracks t JOIN albums al ON al.id = t.album_id WHERE al.artist_id = ar.id)
             ORDER BY LOWER(COALESCE(ar.folder, ar.name))",
        )?;
        let rows = st.query_map([], |r| {
            Ok(Artist { id: r.get(0)?, mbid: r.get(1)?, name: r.get(2)?, folder: r.get(3)?, tracks: r.get(4)? })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn artist_folder(&self, mbid: &str) -> Result<Option<String>> {
        Ok(self
            .0
            .query_row("SELECT folder FROM artists WHERE mbid = ?1", [mbid], |r| r.get::<_, Option<String>>(0))
            .optional()?
            .flatten())
    }

    /// Pin (or with `None` unpin) the folder name used for an artist instead of MusicBrainz's.
    pub fn set_artist_folder(&self, id: i64, folder: Option<&str>) -> Result<()> {
        self.0.execute("UPDATE artists SET folder = ?2 WHERE id = ?1", params![id, folder])?;
        Ok(())
    }

    pub fn external_under(&self, root: &str) -> Result<Vec<ExternalFile>> {
        let mut st = self.0.prepare(&format!("{EXTERNAL_SELECT} WHERE root = ?1"))?;
        let rows = st.query_map([root], external_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Every external file, lossless first, so a FLAC is preferred when a recording is there twice.
    pub fn external_all(&self) -> Result<Vec<ExternalFile>> {
        let mut st = self.0.prepare(&format!("{EXTERNAL_SELECT} ORDER BY (path LIKE '%.flac') DESC, path"))?;
        let rows = st.query_map([], external_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn external_count(&self) -> Result<i64> {
        Ok(self.0.query_row("SELECT COUNT(*) FROM external_files", [], |r| r.get(0))?)
    }

    pub fn upsert_external(&self, e: &ExternalFile) -> Result<()> {
        self.0.execute(
            "INSERT INTO external_files (path, root, size, mtime, recording_mbid, artist, title, duration_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(path) DO UPDATE SET root = excluded.root, size = excluded.size, mtime = excluded.mtime,
                 recording_mbid = excluded.recording_mbid, artist = excluded.artist, title = excluded.title,
                 duration_ms = excluded.duration_ms",
            params![e.path, e.root, e.size, e.mtime, e.recording_mbid, e.artist, e.title, e.duration_ms],
        )?;
        Ok(())
    }

    pub fn delete_external(&self, path: &str) -> Result<()> {
        self.0.execute("DELETE FROM external_files WHERE path = ?1", [path])?;
        Ok(())
    }

    /// Tracks pointing at this absolute path (external ones).
    pub fn tracks_at_path(&self, path: &str) -> Result<Vec<Track>> {
        self.query_tracks(&format!("{TRACK_SELECT} WHERE t.path = ?1 AND t.status = 'external'"), [path])
    }

    /// The track is served by an external file from now on; no download, no own file.
    pub fn mark_external(&self, id: i64, abs_path: &str) -> Result<()> {
        self.0.execute(
            "UPDATE tracks SET status = 'external', path = ?2, source_url = NULL, error = NULL WHERE id = ?1",
            params![id, abs_path],
        )?;
        Ok(())
    }

    pub fn meta_get(&self, key: &str) -> Result<Option<String>> {
        Ok(self.0.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0)).optional()?)
    }

    pub fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        self.0.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn track_by_id(&self, id: i64) -> Result<Option<Track>> {
        Ok(self.query_tracks(&format!("{TRACK_SELECT} WHERE t.id = ?1"), [id])?.pop())
    }

    pub fn tracks_with_path(&self, rel_path: &str) -> Result<Vec<Track>> {
        self.query_tracks(&format!("{TRACK_SELECT} WHERE t.path = ?1 ORDER BY t.id"), [rel_path])
    }

    /// Paths that more than one row claims; should be empty.
    pub fn shared_paths(&self) -> Result<Vec<(String, i64)>> {
        let mut st = self
            .0
            .prepare("SELECT path, COUNT(*) FROM tracks WHERE path IS NOT NULL GROUP BY path HAVING COUNT(*) > 1")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn playlists_with_track(&self, id: i64) -> Result<Vec<i64>> {
        let mut st = self.0.prepare("SELECT playlist_id FROM playlist_tracks WHERE track_id = ?1")?;
        let rows = st.query_map([id], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Back to wanted with no file and no remembered source, whatever state it was in: the next
    /// download searches afresh. `td source` is how a URL is pinned on purpose.
    pub fn reset_track(&self, id: i64) -> Result<()> {
        self.0.execute(
            "UPDATE tracks SET status = 'wanted', path = NULL, source_url = NULL, error = NULL WHERE id = ?1",
            [id],
        )?;
        Ok(())
    }

    pub fn delete_track(&self, id: i64) -> Result<()> {
        self.0.execute("DELETE FROM playlist_tracks WHERE track_id = ?1", [id])?;
        self.0.execute("DELETE FROM tracks WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn retry_failed(&self) -> Result<usize> {
        Ok(self.0.execute("UPDATE tracks SET status = 'wanted', error = NULL WHERE status = 'failed'", [])?)
    }

    pub fn status_counts(&self) -> Result<Vec<(String, i64)>> {
        let mut st = self.0.prepare("SELECT status, COUNT(*) FROM tracks GROUP BY status")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn upsert_playlist(&self, lb_mbid: &str, name: &str, kind: &str, date: &str) -> Result<i64> {
        Ok(self.0.query_row(
            "INSERT INTO playlists (lb_mbid, name, kind, date) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(lb_mbid) DO UPDATE SET name = excluded.name, date = excluded.date,
                 fetched_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') RETURNING id",
            params![lb_mbid, name, kind, date],
            |r| r.get(0),
        )?)
    }

    pub fn set_playlist_tracks(&self, playlist_id: i64, track_ids: &[i64]) -> Result<()> {
        self.0.execute("DELETE FROM playlist_tracks WHERE playlist_id = ?1", [playlist_id])?;
        let mut st =
            self.0.prepare("INSERT INTO playlist_tracks (playlist_id, track_id, position) VALUES (?1, ?2, ?3)")?;
        for (pos, tid) in track_ids.iter().enumerate() {
            st.execute(params![playlist_id, tid, pos as i64])?;
        }
        Ok(())
    }

    pub fn set_playlist_m3u(&self, playlist_id: i64, path: &str) -> Result<()> {
        self.0.execute("UPDATE playlists SET m3u_path = ?2 WHERE id = ?1", params![playlist_id, path])?;
        Ok(())
    }

    pub fn set_playlist_date(&self, playlist_id: i64, date: &str) -> Result<()> {
        self.0.execute("UPDATE playlists SET date = ?2 WHERE id = ?1", params![playlist_id, date])?;
        Ok(())
    }

    pub fn clear_playlist_m3u(&self, playlist_id: i64) -> Result<()> {
        self.0.execute("UPDATE playlists SET m3u_path = NULL WHERE id = ?1", [playlist_id])?;
        Ok(())
    }

    pub fn playlist_tracks(&self, playlist_id: i64) -> Result<Vec<Track>> {
        self.query_tracks(
            &format!("{TRACK_SELECT} JOIN playlist_tracks pt ON pt.track_id = t.id WHERE pt.playlist_id = ?1 ORDER BY pt.position"),
            [playlist_id],
        )
    }

    pub fn playlists(&self) -> Result<Vec<Playlist>> {
        let mut st = self.0.prepare(
            "SELECT p.id, p.lb_mbid, p.name, p.kind, p.m3u_path, p.fetched_at,
                    (SELECT COUNT(*) FROM playlist_tracks WHERE playlist_id = p.id), p.date
             FROM playlists p ORDER BY p.fetched_at DESC, p.id DESC",
        )?;
        let mut out: Vec<Playlist> = st
            .query_map([], |r| {
                Ok(Playlist {
                    id: r.get(0)?,
                    lb_mbid: r.get(1)?,
                    name: r.get(2)?,
                    kind: r.get(3)?,
                    m3u_path: r.get(4)?,
                    fetched_at: r.get(5)?,
                    track_count: r.get(6)?,
                    date: r.get(7)?,
                    synced: vec![],
                })
            })?
            .collect::<Result<_, _>>()?;
        let mut st =
            self.0.prepare("SELECT target, remote_id, synced_at FROM playlist_syncs WHERE playlist_id = ?1")?;
        for p in &mut out {
            p.synced = st
                .query_map([p.id], |r| {
                    Ok(SyncRecord { target: r.get(0)?, remote_id: r.get(1)?, synced_at: r.get(2)? })
                })?
                .collect::<Result<_, _>>()?;
        }
        Ok(out)
    }

    /// Find a playlist by numeric id, or the most recently fetched one of a kind.
    pub fn find_playlist(&self, key: &str) -> Result<Option<Playlist>> {
        let all = self.playlists()?;
        Ok(match key.parse::<i64>() {
            Ok(id) => all.into_iter().find(|p| p.id == id),
            Err(_) => all.into_iter().find(|p| p.kind == key),
        })
    }

    pub fn cached_lookup(&self, path: &str, size: i64, mtime: i64) -> Result<Option<String>> {
        Ok(self
            .0
            .query_row(
                "SELECT recording_mbid FROM import_lookups WHERE path = ?1 AND size = ?2 AND mtime = ?3",
                params![path, size, mtime],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn cache_lookup(&self, path: &str, size: i64, mtime: i64, mbid: &str) -> Result<()> {
        self.0.execute(
            "INSERT INTO import_lookups (path, size, mtime, recording_mbid) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(path) DO UPDATE SET size = excluded.size, mtime = excluded.mtime, recording_mbid = excluded.recording_mbid",
            params![path, size, mtime, mbid],
        )?;
        Ok(())
    }

    pub fn clear_sync(&self, playlist_id: i64, target: &str) -> Result<()> {
        self.0.execute(
            "DELETE FROM playlist_syncs WHERE playlist_id = ?1 AND target = ?2",
            params![playlist_id, target],
        )?;
        Ok(())
    }

    pub fn record_sync(&self, playlist_id: i64, target: &str, remote_id: &str) -> Result<()> {
        self.0.execute(
            "INSERT INTO playlist_syncs (playlist_id, target, remote_id) VALUES (?1, ?2, ?3)
             ON CONFLICT(playlist_id, target) DO UPDATE SET remote_id = excluded.remote_id, synced_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
            params![playlist_id, target, remote_id],
        )?;
        Ok(())
    }

    fn query_tracks<P: rusqlite::Params>(&self, sql: &str, p: P) -> Result<Vec<Track>> {
        let mut st = self.0.prepare(sql)?;
        let rows = st.query_map(p, track_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let dir = std::env::temp_dir().join(format!("tapedeck-db-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let db = Db::open(&dir.join("t.db")).unwrap();

        let new = NewTrack {
            recording_mbid: "rec-1",
            title: "Title",
            track_no: Some(3),
            artist: "A feat. B",
            album_artist_mbid: "art-1",
            album_artist: "A",
            album_mbid: "alb-1",
            album: "Album",
            year: Some(2020),
            artist_mbid: Some("art-1"),
            duration_ms: Some(241_000),
        };
        let id = db.insert_track(&new).unwrap();
        assert_eq!(db.insert_track(&new).unwrap(), id, "same recording MBID must not create a second row");
        assert_eq!(db.wanted_tracks().unwrap().len(), 1);

        let pid = db.upsert_playlist("pl-1", "Weekly Jams for u, week of X", "weekly-jams", "2026-10-05").unwrap();
        assert_eq!(db.upsert_playlist("pl-1", "renamed", "weekly-jams", "2026-10-05").unwrap(), pid);
        db.set_playlist_tracks(pid, &[id]).unwrap();
        db.mark_ready(id, "A/Album (2020)/03 - Title.opus", Some("https://y/v")).unwrap();

        let t = &db.playlist_tracks(pid).unwrap()[0];
        assert_eq!((t.status.as_str(), t.artist.as_str(), t.album_artist.as_str()), ("ready", "A feat. B", "A"));
        assert_eq!(t.path.as_deref(), Some("A/Album (2020)/03 - Title.opus"));
        assert_eq!(
            (t.source_url.as_deref(), t.duration_ms, t.album_mbid.as_str()),
            (Some("https://y/v"), Some(241_000), "alb-1")
        );
        assert!(db.set_source_url(id, "https://y/other").unwrap());
        assert!(!db.set_source_url(999, "x").unwrap());
        assert_eq!(db.wanted_tracks().unwrap()[0].source_url.as_deref(), Some("https://y/other"));
        db.mark_ready(id, "A/Album (2020)/03 - Title.opus", None).unwrap();
        assert!(db.wanted_tracks().unwrap().is_empty());

        db.record_sync(pid, "plex", "42").unwrap();
        let p = db.find_playlist("weekly-jams").unwrap().unwrap();
        assert_eq!((p.name.as_str(), p.track_count), ("renamed", 1));
        assert_eq!(p.synced[0].remote_id.as_deref(), Some("42"));
        assert!(db.find_playlist("daily-jams").unwrap().is_none());

        assert_eq!(db.search("feat").unwrap().len(), 1, "track artist is searchable");
        db.mark_failed(id, "boom").unwrap();
        assert_eq!(db.status_counts().unwrap(), vec![("failed".to_string(), 1)]);
        assert_eq!(db.retry_failed().unwrap(), 1);
        assert_eq!(db.wanted_tracks().unwrap()[0].error, None);

        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
