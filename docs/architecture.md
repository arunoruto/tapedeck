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
  tui.rs           `td tui` / plain `td`: ratatui browser; actions reuse the CLI commands
  theme.rs         TUI colours from TOML; built-ins in themes/
  external.rs      read-only external libraries (Lidarr): index, dedupe, reconcile
  api.rs           HTTP API (POST /cmd, GET /stream) and the client side of [remote]
  player.rs        mpv over JSON IPC for the TUI
  request.rs       song requests: analyse a link, judge, fetch, file, Requests playlist
  telegram.rs      the Telegram bot (long polling) on top of request.rs
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
4. Write one current m3u8 per playlist kind (`playlists/weekly-jams.m3u8`), replaced on
   every run; the full ListenBrainz title is inside the file. The playlist it replaces is
   written to `archive/<kind>/<period>.m3u8` (`archive/weekly-jams/2026-w41.m3u8`, ISO week
   for weekly kinds, the date for daily ones). Playlist folders and files are lowercase slugs;
   the library's own paths follow `path_template` and the targets show `playlist_title`.
   When ListenBrainz regenerates a week (two playlists, same period), the newer one is current
   and the earlier edition is not archived; only a different period is history. An older folder
   layout (`Playlists/`, dated or long-titled files, capitalised backups) is moved into this one
   on the first sync, nothing deleted and nothing overwritten: two backups whose titles slug the
   same keep both, the second with its remote id.
5. Push each of those playlists to every enabled sync target and record the result in
   `playlist_syncs`.

Tracks that drop out of a playlist are never deleted or pruned: they stay as ordinary
library tracks, and only the playlist membership changes. The music lives on the device;
the playlists are what moves.

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

`td retag ID --recording MBID` is the manual override for a wrong match: the row becomes that
recording, metadata is refetched, the file retagged and moved if its templated path changed, and
its playlists' m3u8s rewritten, and playlist backups follow the moved file, as they do after
`td refile` moves. In the TUI it is `m` on a track. `td refile` cannot do this, it
re-resolves the recording id the row already has.

`td import DIR` is for a folder that predates tapedeck. Each audio file is identified by its
`MUSICBRAINZ_TRACKID` tag when present, otherwise by a MusicBrainz search on artist and
title (from tags, else from an `Artist - Title` file name) scored by how close the file's
length is to the recording's. A resolved recording more than 20 s away from the file's
own length is not accepted, whether the tag or the search named it: the search is asked again
for the file's length, and if that finds nothing the file is a miss rather than a wrong cut.
A file the search cannot place still joins the group of a
resolved file with the same artist, title and length, which is how an untagged copy of a tagged
file finds its recording. Files are then grouped by recording, because Explo drops the same
song into every weekly folder it appears in. A recording the library already has makes
every copy a duplicate; otherwise the copy closest to the recording's length is filed exactly
like a download would be and the rest are duplicates. Duplicates are reported, or deleted with
`--apply --move --delete-duplicates`. The default is a dry run, which reports the same groups
`--apply` would act on. `--apply` copies the files in and never modifies or deletes anything
in the source: the least surprising default for a folder another tool still owns. `--move`
moves them instead (then `--delete-duplicates` is allowed), and needs write access to the
source, which is checked before the long resolving pass. A file is tagged only once it sits in the
library, so a failed move or copy leaves the source and the database as they were, is counted
as failed, and the run goes on. Search results are cached per file (path, size, mtime), so the
dry run pays for MusicBrainz once and `--apply` reuses them. Two different recordings can render the same path (two versions of one song on one single):
a destination is never overwritten; if the file already there has the same length the newcomer
is a duplicate of it, otherwise it gets " (2)" before the extension. `td status` warns if two
rows ever end up sharing a file, and `td rm` keeps the file while another row still uses it.
Afterwards `--apply` rewrites any
playlist backup
under `<playlist_dir>/backup` whose paths pointed at files that were moved, copied or deleted (every copy of a recording
points at the kept one), so `td remote restore` still finds them.

`td push` takes a fetched playlist (id or kind) or any m3u8 file: the archive, a backup,
something hand-written. File lines are resolved against the file's folder, lines that are not
on disk are reported and skipped, and the title comes from `--title`, the file's database row
(the kind's title for the current playlist, the ListenBrainz title for an archived one), its
`#PLAYLIST` line, or the file name, in that order. Before any push the same-titled remote
playlist, if there is one, is backed up like `remote rm` does. `td unpush` removes a playlist
from the targets only, backup first; nothing on disk changes. In the TUI these are `p` and `u`
on a playlist row, and a `✓ plex` mark shows which targets currently hold a playlist under
that title.

`td remote relink --from DIR` is for the end of a migration: every distinct backup entry under
`DIR` (or anywhere outside the library, without `--from`) is resolved to a library track, by
the file's MusicBrainz tag, by the cached import lookup, or by artist, title and length against
an index of the library built once, and rewritten. Smart playlists' backups are left alone by
relink (Plex regenerates them from a filter), but moves made by import, refile and retag are
followed in them too, since that is harmless.
Backup files are replaced atomically. After that the imported folder can go without any
backup dangling.

`td remote ls` and `td remote rm GLOB` talk to the sync targets' own playlist lists, so the
history another tool left behind (`Weekly-Jams-2026-Week*`) can be cleared without opening
Plex. `rm` only lists matches until `--yes` is given, skips the server's own smart playlists
unless told otherwise, and first writes each playlist as an m3u8 (local paths) plus a JSON
sidecar under `<playlist_dir>/backup/<target>/<slug>.m3u8` (the real title stays in the JSON;
a slug already taken by a playlist with another id gets the id appended, so "Weekly Jams" and
Explo's "Weekly-Jams" keep separate backups, and the backup a push writes beforehand already
carries the paths of files this run moved); `td remote backup` does only that, and
`td remote restore FILE.json` recreates a playlist from the sidecar.

Long loops (import's resolving pass, refile) report a `progress` event per item. On a terminal
it overwrites itself on one line; in a pipe or the journal it is dropped; with `--json` it is one
more NDJSON line, so a script can show its own counter.

## Playback

`player.rs` drives mpv over its JSON IPC socket: `mpv --no-video --idle` is started on the
first play with `--input-ipc-server`, given file paths (local) or `/stream/<id>` URLs (remote,
with the bearer header fixed at launch), and polled for position and state every other TUI
tick. The queue is tapedeck's, so the bar shows "Artist – Title" rather than a URL. Nothing
decodes audio in tapedeck; mpv handles formats, gapless playback and prefetching. Scrobbling
follows ListenBrainz's rule from the same poll: the TUI fires `listen --now` when the track id
changes and `listen` once position passes half the length or four minutes, and the daemon
submits with its token, queueing failures in `listens` and flushing them at the next listen
or start. That keeps the token on the server and makes the `listen` scope safe to leave open.

## Remote use

`api.rs`. `[server] listen` adds an HTTP face to the daemon with the socket's protocol:
`POST /cmd` takes a `Command` and streams `Event`s as NDJSON, `GET /stream/<id>` serves a
track's file with Range support (tower-http's `ServeFile`). A bearer token, when configured,
grants everything; without it a request gets the `[server] anonymous` scopes (`scope_of` maps
each command to read, request, sync or admin), and a command outside them is a `403` naming
what it needs. On a
client, `[remote] url` makes `daemon::dispatch` send every command there instead of the socket
or an in-process `App`, and the TUI loads its data through commands (`tracks`, `playlists`,
`playlist-tracks`, `remote-playlists`, hidden from `--help`) rather than opening the database,
so the same binary works against a local or a remote daemon.

## Song requests

`request.rs` is the pipeline, `telegram.rs` one way in, `td request` the other. A link is
probed with `yt-dlp --dump-json`; the title is cleaned of "(Official Video)"-style noise and
split into artist and title with the channel name deciding which side is which, YouTube
Music's own track/artist fields win when present. MusicBrainz is asked for candidates with
the same ranking the sync uses, and `judge` says whether the top one can be taken without
asking: same normalised title, compatible artist, identical version words (remix, live, …),
uploaded by the artist or a label/Topic channel, and for a music track a length within 20 s.
Fulfilling inserts the recording's row, downloads through the usual YouTube-Music-first path
(falling back to the link's own audio when that fails), files it, and prepends it to the
`requests-<year>` playlist, which is an ordinary playlist kind, so it gets its own current
m3u8 and Plex playlist and the sync's archive logic never touches it. "Exactly this video"
files the link as sent under a `youtube:<id>` recording id; refile skips such rows. The bot
keeps open questions in memory keyed by its own message ids; buttons, 👍/👎 reactions and
`/wrong` replies all route back to them, and a confirmation is written to `requests`. Every fulfilment runs under the daemon's busy lock.

## External libraries

`external.rs`. Folders someone else manages (Lidarr) are indexed into `external_files` by
recording id, artist, title and length, incrementally by size and mtime, with a one-minute
settle time for files still being written. The index is consulted wherever a recording would
otherwise be fetched or kept: the sync marks a wanted track `external` (status `external`,
`path` absolute) instead of downloading, import treats the recording as a duplicate, and
`td external dedupe` swaps an own copy for the external one. A scan also reconciles: a vanished
file sends its tracks to another external copy or back to `wanted`, a new file covers wanted
and failed tracks at once and own copies according to `prefer_external`, and playlists whose
paths changed are pushed again. The daemon scans on `external.scan_interval` and at every sync.
Everything that writes files (refile, retag, retry, rm) checks `is_external` first and leaves
those alone; m3u8 and Plex see the external path like any other.

## Running as a service

The daemon is one process: a Unix socket for commands, a ticker that runs `sync` every
`sync.interval` (immediately on start unless `sync.on_start = false`), and a second ticker for
the external-library scan. Events of socket-driven commands are logged to the journal too, so
a sync started from the TUI is traceable afterwards. Two modules wrap it.
The Home Manager one is a user unit for a desktop. The NixOS one is for a server: its own
system user and group, `StateDirectory`/`RuntimeDirectory` with mode 0770 and `UMask=0002` so
group members can use the CLI and TUI against the running daemon and the files it writes are
group-writable for Plex and Samba, `TAPEDECK_CONFIG` pointing every shell at the system config,
and the usual systemd sandboxing with the library and playlist folder as the only writable
paths.

## TUI

`td tui` (and plain `td`) opens the database directly (no daemon round-trip) and fetches each
sync target's playlist list once at start. Two tabs. *Library* is a three-column browser,
artists › albums › songs, regrouped from the track table on every draw (cheap at this size;
cache it if that ever shows). *Playlists* lists local playlists with their sync marks and remote
playlists dimmed, with the tracks of the selected one on the right. `/` filters by artist, title
or album in both tabs; the pane below shows the selected track's id, path, source URL and error,
plus the last lines of whatever action is running. Colours come from a `Theme` (`theme.rs`): a small TOML of named colours, two Catppuccin
flavours compiled in and user files under `<config dir>/themes/` taking precedence, selected by
`theme` in the config. The TUI never sets a background, so it sits on whatever the terminal
has. The header is a four-line cassette drawn from the theme's stripe colours, with the tabs and
the library's size beside it, nothing else. `assets/demo.tape` is a [vhs](https://github.com/charmbracelet/vhs)
script that records the README's GIF and three PNGs against a scratch config: run
`TAPEDECK_CONFIG=... vhs assets/demo.tape` with `td` on `PATH`. Keys follow vim and less: `hjkl`, `^D`/`^U`
half page, `^F`/`^B` page, `g`/`G`, `Tab`, `[`/`]` or `1`/`2` for tabs. `i` (or `Enter` on a
track) opens a popup with the tags, what is on disk (format, bitrate, sample rate, length, size,
read from the file's header on the spot, so it reflects the real download), the source URL, and
the ListenBrainz player, MusicBrainz recording, release and artist URLs. Playback is not in the
TUI; those links are how a track is checked for now.

Actions are the CLI's own `Command`s, nothing more: `S` is `td sync`, `R` is `td retry ID`
followed by `td sync --no-fetch`, `s` asks for a URL and runs `td source` plus the same sync,
`m` asks for a recording id and runs `td retag`, `p` is `td push` and `u` is `td unpush`, `d` is `td rm ID` for a track or `td remote rm TITLE --yes` for a remote
playlist (backup first, as always). They go through `daemon::dispatch`, so with the service
running the daemon does the work under its lock and the TUI only streams the events; without
it the command runs in-process. One action at a time. Everything that deletes or re-downloads
asks `[y/N]` first, including `S` and the source prompt after its URL, so a stray key never
starts a job, and the TUI reloads from the database when the action ends. Local playlists
cannot be deleted from here: they mirror ListenBrainz and would come back on the next sync.

## Data model

```
artists         id, mbid, name, folder
albums          id, mbid, artist_id, title, year
tracks          id, recording_mbid, album_id, title, artist, artist_mbid, track_no, duration_ms,
                path, status, source_url, error
playlists       id, lb_mbid, name, kind, m3u_path, date, fetched_at
requests        track_id, source, requested_at, confirmed_at
external_files  path, root, size, mtime, recording_mbid, artist, title, duration_ms
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

**Which upload gets downloaded.** Explo's weak spot was the music video: a six-minute upload
with three minutes of song. tapedeck asks YouTube Music's song results first (album audio,
real lengths), looks the top hits up once more for their durations and takes the one within
20 s of the MusicBrainz recording, preferring an exactly matching title; only when none fits
does it fall back to the plain YouTube search with the same closest-length rule. After the
download the file's own length is checked against the recording and anything more than 20 s
off is deleted and reported as failed, so a wrong cut never lands in the library. Both
candidates are tried in turn, since YouTube refuses the odd download with a 403. `td retry ID`
drops the remembered URL and searches afresh; `td source ID URL` pins a URL for the cases
nothing automatic gets right.

**yt-dlp as a subprocess, not a library.** It changes weekly to keep up with sites;
shelling out means `yt-dlp -U` fixes things without a rebuild.

**One current m3u8 per playlist kind, plus an archive.** `playlists/weekly-jams.m3u8`
mirrors what Plex shows: the current week, replaced on every run, usable by any player
without tapedeck. The week it replaced moves to `playlists/archive/weekly-jams/2026-w41.m3u8`,
kept just in case and never pushed unless asked. The database holds the same history
(`td playlists`, the TUI), and the tracks themselves stay in the library whatever the
playlists do.

**SQLite only, no JSON store.** One source of truth. `--json` on any command gives a
machine-readable dump when needed.

**Track artist and album artist are stored separately.** `tracks.artist` is the
recording's credit (`Bonobo feat. Nicole Miglis`); `artists` holds the artist entity
(`Bonobo`, by MusicBrainz id) and is what albums hang off.

**Artist folders follow the MusicBrainz entity, with a per-artist pin.** Bands rename
(Eskimo Callboy → Electric Callboy) and a folder per spelling is the thing nobody wants, so the
folder name is the entity's current name, refreshed on every lookup; `td refile` reports
"artist folder changed: X → Y (N tracks)" (or "would change" in a dry run) and moves the files. When the entity name is not what
belongs on disk (Miley Cyrus became "MILEY" in 2026), `td artist "MILEY" --folder "Miley Cyrus"`
pins a name for that artist (`--unpin` follows MusicBrainz again); `td artists` lists both
columns. Pins live in `artists.folder` and win everywhere the album artist is rendered. Filing by `{album_artist}` keeps an album in one
folder even when half its tracks have guests.

**Plex playlists are recreated, not diffed.** Deleting and recreating is a handful of
requests and has no edge cases. Playlist history lives in the database, not in Plex.

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
