# Commands

| Command | What it does |
|---|---|
| `td sync [--playlist KIND] [--no-push] [--no-fetch]` | Fetch playlists, download what's missing, tag, file, write m3u8, push; `--no-fetch` skips ListenBrainz and only downloads what is already wanted |
| `td status` | Track counts by status (`wanted` / `ready` / `failed`) |
| `td search QUERY` | Search the library by artist, album or title |
| `td playlists` | Fetched playlists and where they've been synced |
| `td artists` / `td artist NAME --folder F` | Album artists as MusicBrainz names them now; pin a folder name where you disagree |
| `td push PLAYLIST\|FILE.m3u8 [--to TARGET] [--title T]` | Push a fetched playlist, or any m3u8 file (archive, backup, your own); a same-titled remote copy is backed up and replaced |
| `td unpush NAME\|FILE.m3u8 [--to TARGET] --yes` | Remove a playlist from a target, backup first; the local file is never touched |
| `td retry [ID]` | Reset failed downloads so the next sync tries again; with an id, re-download that one track |
| `td rm ID` | Delete one track's file and forget it |
| `td source ID URL` | Pin the download URL for one track and queue it |
| `td retag ID --recording MBID` | Correct a wrong match: make the track that recording, retag, move if the path changes |
| `td refile [--dry-run]` | Re-check every track against MusicBrainz, retag, move files whose path changed |
| `td lengths [TRACK] [--list] [--verdict V]` | Files whose length is off their recording by more than max(10 s, 5 %), sorted into padded, external, metadata (a better-fitting recording to relink to), release (Deezer has a version this long, so MusicBrainz is off), broken, longer and shorter. Stored for review; `--list` shows the stored audit without checking again; good ones are hidden unless `--verdict good` (or `all`) |
| `td lengths TRACK --mark good\|bad\|relink\|none` | Judge a finding after listening (the TUI's Review tab does the same with `g` `b` `r` `u`) |
| `td lengths [TRACK] --fix` | Act on the verdicts only: bad files are downloaded again (kept only when closer, old file under `<library>/.tapedeck-backup/`), relink ones retagged |
| `td import DIR [--apply] [--move] [--delete-duplicates]` | Bring an existing folder (say, Explo's downloads) into the library; dry run by default, `--apply` copies and leaves the source alone, `--move` moves |
| `td remote ls` / `td remote rm GLOB [--yes]` | List or bulk-delete playlists on a sync target; `rm` backs up first |
| `td remote backup [GLOB]` / `td remote restore FILE` | Save target playlists as m3u8 + JSON, or recreate one from such a file |
| `td request URL\|"Artist - Title" [--pick N] [--video]` | Request one song: identify, fetch, file, add to this year's Requests playlist |
| `td lyrics fetch [--all] [--external] [--force] [TRACK]` / `show TRACK` / `status` / `reject TRACK` / `check [--fix]` | Lyrics from LRCLIB as sidecars and in the database; see [Lyrics](lyrics.md) |
| `td steam list` / `td steam sync` | Steam soundtracks: what the client installed or the account owns, and import the new ones; see [Steam soundtracks](steam.md) |
| `td external scan` / `td external dedupe [--dry-run]` | Index read-only libraries (Lidarr) and replace own copies of recordings they hold |
| `td remote relink --from DIR` | Point backup entries under DIR at the library's copy of the same recording; run before deleting an imported folder |
| `td daemon` | Run as a service: listen on the socket, sync every `sync.interval` |
| `td` / `td tui` | Browse the library (artist › album › song) and playlists; `i` shows a track's tags, file quality and MusicBrainz/ListenBrainz/source URLs; sync, re-download, pin a source, fix a match, push, delete, each behind a `[y/N]`. Catppuccin out of the box, your own colours via a TOML theme |

Output is a table or plain lines; add `--json` to any command for newline-delimited JSON
events instead, which is what scripts and agents should parse.

Every command sends newline-delimited JSON events with `--json`. With a daemon running, a
command goes through its socket (or, with `[remote] url`, its HTTP API); without one it runs
in-process. Read-only commands never wait for a running sync.
