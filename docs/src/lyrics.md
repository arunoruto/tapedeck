# Lyrics

tapedeck fetches lyrics from [LRCLIB](https://lrclib.net), an open database of synced and
plain lyrics that needs no account, and keeps them in two places: as sidecar files next to
the audio, which is the only way Plex and Plexamp read lyrics, and in its own database, which
is what [Subsonic clients](subsonic.md) and the TUI use. It is on by default and asks for
nothing: LRCLIB is open data, and files are written only into tapedeck's own library.

## What happens

- After every download, and on `[lyrics] interval` (default every six hours) for anything
  still without lyrics, tapedeck looks each track up by artist, title, album and length, then
  by search as a fallback, one request a second. A miss is remembered and asked again after
  `retry_after` (default thirty days), because LRCLIB grows.
- Synced lyrics become `<same name>.lrc`, plain ones `<same name>.txt`, written atomically next
  to the file. An existing sidecar is never overwritten (unless `--force`) and is indexed
  instead, so a library that already has `.lrc` files is simply read. Audio files are never
  touched. Instrumentals are marked and get no file.
- Plex is asked to refresh the metadata of each album that gained a sidecar, so Plexamp shows
  the lyrics without a manual refresh.
- Sidecars follow their track: `td retag`'s moves, renames, deletes and `td external dedupe`
  take the `.lrc`/`.txt` along.
- Subsonic clients get synced lyrics through the OpenSubsonic `songLyrics` extension
  (`getLyricsBySongId`) and plain ones through `getLyrics`, for every track with lyrics in the
  database, including external ones nothing was written next to.

## External libraries

For tracks in [external libraries](external.md) such as Lidarr's, the default is
`external = "db-only"`: lyrics go into the database only, so Subsonic apps and the TUI show
them, Plexamp does not, and nothing is written into folders tapedeck does not own. Lidarr's
own `.lrc` files are read and indexed either way.

Set `external = "missing-sidecars"` to also write `.lrc`/`.txt` next to external files that
have none. Existing sidecars and audio are still never touched. On NixOS the external paths
then need to be in the service's `ReadWritePaths`, and when Lidarr renames or upgrades a track
the old sidecar stays behind until you clean it up.

## Commands

```sh
td lyrics status              # coverage per library: found, missing, instrumental, untried, sidecars
td lyrics show 123            # what is known for one track, and whether a sidecar is on disk
td lyrics fetch               # tracks without lyrics that are due (what the daemon runs)
td lyrics fetch --all         # every track without lyrics, ignoring the retry schedule
td lyrics fetch --external    # only external tracks
td lyrics fetch 123 --force   # ask again and overwrite the sidecar
```

## Configuration

```toml
[lyrics]
enable = true
write_sidecars = true         # .lrc/.txt next to own tracks
external = "db-only"          # or "missing-sidecars"
retry_after = "30d"
interval = "6h"               # daemon pass; "0" turns it off
```

LRCLIB is the only source. Musixmatch, Genius, Spotify and Apple need scraped tokens or break
their terms, so they are out; a further open source can be added behind `[lyrics]` later.
