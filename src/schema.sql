PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS artists (
    id   INTEGER PRIMARY KEY,
    mbid TEXT UNIQUE,
    name TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS albums (
    id        INTEGER PRIMARY KEY,
    mbid      TEXT UNIQUE,
    artist_id INTEGER NOT NULL REFERENCES artists(id),
    title     TEXT NOT NULL,
    year      INTEGER
);

CREATE TABLE IF NOT EXISTS tracks (
    id             INTEGER PRIMARY KEY,
    recording_mbid TEXT UNIQUE NOT NULL,
    album_id       INTEGER NOT NULL REFERENCES albums(id),
    title          TEXT NOT NULL,
    artist         TEXT NOT NULL,                          -- track artist credit; albums.artist_id is the album artist
    artist_mbid    TEXT,                                   -- first credited artist of the recording
    duration_ms    INTEGER,                                -- MusicBrainz recording length
    track_no       INTEGER,
    path           TEXT,                                   -- relative to library root
    status         TEXT NOT NULL DEFAULT 'wanted',         -- wanted | ready | failed
    source_url     TEXT,                                   -- explicit download URL, else the downloader searches
    error          TEXT,
    added_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE TABLE IF NOT EXISTS playlists (
    id         INTEGER PRIMARY KEY,
    lb_mbid    TEXT UNIQUE NOT NULL,
    name       TEXT NOT NULL,
    kind       TEXT NOT NULL,                              -- ListenBrainz source_patch: weekly-jams, daily-jams, ...
    m3u_path   TEXT,
    fetched_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE TABLE IF NOT EXISTS playlist_tracks (
    playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
    track_id    INTEGER NOT NULL REFERENCES tracks(id),
    position    INTEGER NOT NULL,
    PRIMARY KEY (playlist_id, track_id)
);

CREATE TABLE IF NOT EXISTS playlist_syncs (
    playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
    target      TEXT NOT NULL,
    remote_id   TEXT,
    synced_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    PRIMARY KEY (playlist_id, target)
);

-- Search results of `td import`, so a re-run (dry run, then --apply) does not query
-- MusicBrainz again for the same unchanged file. Tagged files never need this.
CREATE TABLE IF NOT EXISTS import_lookups (
    path           TEXT PRIMARY KEY,
    size           INTEGER NOT NULL,
    mtime          INTEGER NOT NULL,
    recording_mbid TEXT NOT NULL
);
