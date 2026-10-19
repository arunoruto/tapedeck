# External libraries

External files are also served to [Subsonic clients](subsonic.md), with [lyrics](lyrics.md)
from the database, so a Lidarr library is browsable and playable through tapedeck too.

Music managed by something else, say Lidarr's folder, is often better quality than a YouTube
rip and should not be downloaded again. List those folders and tapedeck treats them as
read-only sources of recordings:

```toml
[external]
paths = ["/mnt/flash/music/releases", "/mnt/flash/music/singles"]
scan_interval = "1h"        # the daemon rescans on its own clock; "0" turns that off
prefer_external = "ask"     # auto | ask | never, see below
```

| Key | Default | Meaning |
|---|---|---|
| `paths` | `[]` | Folders to index. Nothing under them is ever written, moved, retagged or deleted. |
| `scan_interval` | `1h` | How often the daemon rescans them, independently of `sync.interval`. Every sync scans too. `0` disables the periodic scan. |
| `prefer_external` | `ask` | What happens when a scan finds an external copy of a track the library already has. `auto`: point the track at the external file and delete the own copy. `ask`: only report it; `td external dedupe` does the swap. `never`: leave own copies alone. |

Files are indexed by their MusicBrainz recording id (Picard and Lidarr write it) with a
fallback on artist, title and length, incrementally by size and modification time, and a file
changed in the last minute is left for the next scan so a half-copied FLAC is not indexed. A
playlist track whose recording is in the index is marked `external` and never downloaded; its
m3u8 line and its Plex entry use the external file's path. When an external file moves (Lidarr
upgrading an MP3 to a FLAC under a new name) the track follows it and the affected playlists
are pushed again; when it vanishes with no other copy the track becomes `wanted` and the next
sync downloads it. `td external scan` runs the scan by hand, `td external dedupe [--dry-run]`
replaces own tracks that exist externally, `td import` treats such recordings as duplicates,
and `td status` shows how many files are indexed and when. Refile, retag, retry and rm never
touch an external file.

