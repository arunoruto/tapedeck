# Architecture

Single Rust crate, one binary (`td`) that is both the CLI and the daemon.

```
src/
  main.rs          parse args; connect to the daemon or run in-process
  cli.rs           Command enum: clap subcommands, also the socket wire format
  daemon.rs        Unix socket server + periodic sync tick
  app.rs           App: executes a Command, streams Events
  config.rs        TOML config and derived paths
  db.rs            rusqlite wrapper, schema in schema.sql
  library.rs       path template, tagging (lofty), moving files into place
  m3u.rs           m3u8 writer
  listenbrainz.rs  created-for playlists (JSPF)
  musicbrainz.rs   recording lookup
  download/        Downloader trait, yt-dlp implementation
  sync/            SyncTarget trait, Plex implementation
  tui.rs           `td tui` / plain `td`: ratatui browser over the database (read-only)
  render.rs        tables / key-value lines for human output
```

## Sync flow

`td sync` does, in order:

1. Fetch the user's created-for playlists from ListenBrainz, keep the kinds listed in
   `listenbrainz.playlists`, and of each kind only the newest. ListenBrainz keeps weeks of
   history and now and then generates the same week twice.
2. For each playlist, walk its tracks. A recording MBID we already have is reused;
   a new one is looked up on MusicBrainz and inserted as `wanted`. The playlist's
   track list is replaced in `playlist_tracks`.
3. Download every `wanted` track with the configured backend into
   `<library>/.tapedeck-tmp`, write tags from the MusicBrainz data, render the path
   template and rename the file into place. Success → `ready` with its relative path;
   failure → `failed` with the error text. `td retry` flips `failed` back to `wanted`.
4. Write an m3u8 for each playlist fetched in this run, named `<Kind> <date>.m3u8`
   (`Weekly Jams 2026-10-05.m3u8`); the full ListenBrainz title is inside the file.
5. Push each of those playlists to every enabled sync target and record the result in
   `playlist_syncs`.

Steps 2 and 3 are idempotent per MBID. Running sync twice downloads nothing new.

## Daemon and client

`td daemon` binds `socket` and runs the sync flow every `sync.interval`. Every other
`td` command first tries to connect to that socket:

- Connected: the `Command` is serialized as one JSON line, the daemon streams back
  `Event` JSON lines and closes. The client renders them.
- Not connected: the same `App::run` executes in the CLI process.

`Command` is the protocol. There is no second schema to keep in sync with the CLI,
and `--json` output is exactly what goes over the socket.

Events are `info` (progress text), `data` (a command's structured result) and `error`.
A non-zero exit means at least one `error` event was emitted.

The daemon serializes commands with a single lock. A `td search` issued while a sync
is downloading waits for the sync. That is deliberate for now: it keeps SQLite access
trivially correct and the download queue is the slow part anyway.

## Extension points

Two traits, both small on purpose:

```rust
trait Downloader {
    async fn download(&self, track: &Track, dest_dir: &Path) -> Result<PathBuf>;
}

trait SyncTarget {
    async fn sync_playlist(&self, title: &str, files: &[PathBuf]) -> Result<String>;
}
```

A downloader only has to produce a file. Tagging and filing happen afterwards in
`library.rs`, so a Soulseek or Bandcamp backend does not have to know about templates.
A sync target only sees a title and absolute paths. Jellyfin, Subsonic or an
"export m3u to this folder" target are each one file in `sync/`.

Backends are selected by name in `download.backend` and `sync.targets`; the match
statements in `download/mod.rs` and `sync/mod.rs` are the registry.

## Refiling and importing

`td refile` runs every track through the MusicBrainz lookup again, writes the result back
(which is how a ranking fix reaches tracks filed before it), retags the file and moves it if
the templated path changed, pruning directories left empty. The m3u8 files are rewritten
afterwards because their paths may have changed. `--dry-run` prints the moves and touches
nothing. One lookup per track at one request per second, so a thousand tracks take a quarter
of an hour.

`td import DIR` is for a folder that predates tapedeck. Each audio file is identified by its
`MUSICBRAINZ_TRACKID` tag when present, otherwise by a MusicBrainz search on artist and
title (from tags, else from an `Artist - Title` file name) scored by how close the file's
length is to the recording's. Files are then grouped by recording, because Explo drops the
same song into every weekly folder it appears in. A recording the library already has makes
every copy a duplicate; otherwise the copy closest to the recording's length is filed exactly
like a download would be and the rest are duplicates. Duplicates are reported, or deleted with
`--apply --delete-duplicates`. The default is a dry run, which reports the same groups
`--apply` would act on; `--apply` moves files, and afterwards rewrites any playlist backup
under `<playlist_dir>/backup` whose paths pointed at files that moved or were deleted, so
`td remote restore` still finds them.

`td remote ls` and `td remote rm GLOB` talk to the sync targets' own playlist lists, so the
history another tool left behind (`Weekly-Jams-2026-Week*`) can be cleared without opening
Plex. `rm` only lists matches until `--yes` is given, skips the server's own smart playlists
unless told otherwise, and first writes each playlist as an m3u8 (local paths) plus a JSON
sidecar under `<playlist_dir>/backup/<target>/`; `td remote backup` does only that, and
`td remote restore FILE.json` recreates a playlist from the sidecar.

## TUI

`td tui` (and plain `td`) opens the database directly (no daemon round-trip) and fetches each
sync target's playlist list once at start. Two tabs. *Library* is a three-column browser,
artists › albums › songs, regrouped from the track table on every draw (cheap at this size;
cache it if that ever shows). *Playlists* lists local playlists with their sync marks and remote
playlists dimmed, with the tracks of the selected one on the right. `/` filters by artist, title
or album in both tabs; the bar below shows the selected track's id, path, source URL and error.
Keys follow vim and less: `hjkl`, `^D`/`^U` half page, `^F`/`^B` page, `g`/`G`, `Tab` or `1`/`2`
for tabs. It changes nothing, by design, until the actions (retry, pin a source, push, delete)
get confirmation prompts.

## Data model

```
artists         id, mbid, name
albums          id, mbid, artist_id, title, year
tracks          id, recording_mbid, album_id, title, artist, artist_mbid, track_no, duration_ms,
                path, status, source_url, error
playlists       id, lb_mbid, name, kind, m3u_path, fetched_at
playlist_tracks playlist_id, track_id, position
playlist_syncs  playlist_id, target, remote_id, synced_at
```

`tracks.recording_mbid` is unique. That single constraint is what stops the library
filling up with duplicates when the same song shows up in several weekly playlists.
`tracks.path` is relative to `library`, so the library can move.

## Design notes

Choices that are not obvious from the code, and why.

**MusicBrainz is the source of truth for metadata, not yt-dlp.** ListenBrainz hands us
recording MBIDs for free. Looking each one up once gives consistent artist, album, year
and track number, which is what makes a path template and clean tags possible. The
download is just the audio.

**yt-dlp as a subprocess, not a library.** It changes weekly to keep up with sites;
shelling out means `yt-dlp -U` fixes things without a rebuild.

**One m3u8 per fetched playlist, always.** The dated ListenBrainz title is the file
name, so the folder is a history of what was recommended when, usable by any player
without tapedeck. Sync targets get a stable name (`Weekly Jams`) and are replaced each
week, matching what Explo does.

**SQLite only, no JSON store.** One source of truth. `--json` on any command gives a
machine-readable dump when needed.

**Track artist and album artist are stored separately.** `tracks.artist` is the
recording's credit (`Bonobo feat. Nicole Miglis`); `artists` holds the release's credit
(`Bonobo`) and is what albums hang off. Filing by `{album_artist}` keeps an album in one
folder even when half its tracks have guests.

**Plex playlists are recreated, not diffed.** Deleting and recreating is a handful of
requests and has no edge cases. Playlist history lives in the m3u8 files, not in Plex.

**The release picked for a recording** is ranked: official status, then no secondary
types (compilation, live, DJ-mix, soundtrack), then released by the track's own artist,
then album before EP before single, then earliest date, with dates compared as
(year, month, day) so a bare `1993` doesn't beat `1993-02-22` and an unknown date sorts
last. Before the secondary-type and artist checks, *Creep* ended up on a Belgian
compilation and *Nightcall* in a DJ mix.

**Downloads are matched by length.** `ytsearch1:` returns whatever YouTube ranks first,
which is often the music video with a long intro or a radio edit. tapedeck asks for five
hits and takes the one closest to the MusicBrainz recording length; if even that is more
than 20 seconds off it fails the track instead of filing the wrong audio, and `td source`
lets you pin a URL by hand. The chosen URL is kept in `tracks.source_url`.
