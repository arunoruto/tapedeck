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

-- Files in read-only external libraries (Lidarr), indexed so their recordings are never
-- downloaded or kept twice. Rescanned incrementally by (path, size, mtime).
CREATE TABLE IF NOT EXISTS external_files (
    path           TEXT PRIMARY KEY,
    root           TEXT NOT NULL,
    size           INTEGER NOT NULL,
    mtime          INTEGER NOT NULL,
    recording_mbid TEXT,
    artist         TEXT NOT NULL,
    title          TEXT NOT NULL,
    duration_ms    INTEGER
);
CREATE INDEX IF NOT EXISTS external_files_mbid ON external_files(recording_mbid);

CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- Song requests (Telegram, td request): when they came in, where from, and whether the
-- requester confirmed the pick. Rows go with the track.
CREATE TABLE IF NOT EXISTS requests (
    track_id     INTEGER PRIMARY KEY REFERENCES tracks(id) ON DELETE CASCADE,
    source       TEXT,
    requested_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    confirmed_at TEXT
);

-- Listens that could not be submitted to ListenBrainz yet (offline, 5xx); retried on the
-- next listen and at daemon start.
CREATE TABLE IF NOT EXISTS listens (
    id          INTEGER PRIMARY KEY,
    track_id    INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    listened_at INTEGER NOT NULL,
    attempts    INTEGER NOT NULL DEFAULT 0
);

-- `td lengths`: own files whose length is off their recording, and what someone who listened
-- decided (good | bad | relink). A verdict holds while size and mtime stay the same.
CREATE TABLE IF NOT EXISTS lengths (
    track_id   INTEGER PRIMARY KEY REFERENCES tracks(id) ON DELETE CASCADE,
    kind       TEXT NOT NULL,                              -- padded | external | metadata | release | broken | longer | shorter | unreadable
    mb_ms      INTEGER NOT NULL,
    file_ms    INTEGER NOT NULL,
    note       TEXT,
    relink     TEXT,                                       -- recording MBID that fits the file
    size       INTEGER,
    mtime      INTEGER,
    verdict    TEXT,
    decided_at INTEGER,
    checked_at INTEGER NOT NULL
);

-- Lyrics per track: what LRCLIB (or a sidecar already on disk) said. `sidecar` is the
-- extension of the file tapedeck wrote next to the audio (lrc/txt), if any; a miss is retried
-- after [lyrics] retry_after.
CREATE TABLE IF NOT EXISTS lyrics (
    key        TEXT PRIMARY KEY,                           -- t:<track id> | x:<external file path>
    status     TEXT NOT NULL,                              -- found | missing | instrumental
    source     TEXT,                                       -- file | lrclib
    source_id  TEXT,
    method     TEXT,                                       -- file | get | get-primary | search | search-primary
    delta_ms   INTEGER,                                    -- provider length minus ours
    synced     TEXT,
    plain      TEXT,
    sidecar    TEXT,
    fetched_at INTEGER NOT NULL
);
