# Configuration

tapedeck reads one TOML file: `--config PATH`, `$TAPEDECK_CONFIG`, or
`$XDG_CONFIG_HOME/tapedeck/config.toml` (`~/.config/tapedeck/config.toml`).
Unknown keys are an error, so typos fail at startup.

A complete example lives in [`config.example.toml`](../config.example.toml).

## Top level

| Key | Default | Meaning |
|---|---|---|
| `library` | required | Root of the music library. Downloads land under it. |
| `path_template` | `{album_artist}/{album} ({year})/{track:02} - {title}.{ext}` | Where a track is filed, relative to `library`. See below. |
| `playlist_dir` | `<library>/Playlists` | Where m3u8 files are written. |
| `database` | `$XDG_DATA_HOME/tapedeck/tapedeck.db` | SQLite file. |
| `socket` | `$XDG_RUNTIME_DIR/tapedeck.sock` | Daemon socket. Falls back next to the database if `XDG_RUNTIME_DIR` is unset. |

### Path template

Lidarr-style tokens inside `{}`:

| Token | Value |
|---|---|
| `{artist}` | Track artist credit as MusicBrainz prints it (`A feat. B`). |
| `{album_artist}` | Artist credit of the release the track was filed under. Differs from `{artist}` on compilations and guest features. |
| `{album}` | Release title. |
| `{year}` | Release year, or `Unknown`. |
| `{track}` / `{track:02}` | Track number, plain or zero-padded. |
| `{title}` | Recording title. |
| `{ext}` | File extension from the download. |

Every token value is sanitized: `/ \ : * ? " < > |` and control characters become `_`,
so a title can never create a directory. Literal text outside braces is kept as is,
including slashes, which is how you shape the directory tree. An unknown token is a
startup error.

## `[listenbrainz]`

| Key | Default | Meaning |
|---|---|---|
| `user` | required | ListenBrainz username whose created-for playlists to follow. |
| `token` | none | User token. Only needed if the playlists are private. |
| `playlists` | `["weekly-jams", "weekly-exploration"]` | Which generated playlists to track, by ListenBrainz `source_patch` name. `daily-jams` is the other common one. |

## `[download]`

| Key | Default | Meaning |
|---|---|---|
| `backend` | `ytdlp` | Download backend. Only `ytdlp` exists today. |
| `format` | `opus` | Passed to `yt-dlp --audio-format`. |
| `ytdlp_bin` | `yt-dlp` | Executable name or path. |

yt-dlp is searched with `ytsearch1:<artist> - <title>` unless a track has an explicit
`source_url` in the database. ffmpeg must be available for the audio extraction.

## `[sync]`

| Key | Default | Meaning |
|---|---|---|
| `interval` | `6h` | How often the daemon runs `sync`. Suffixes: `s`, `m`, `h`, `d`. |
| `targets` | `[]` | Enabled targets, in push order. Only `plex` exists today. |

### `[sync.plex]`

| Key | Meaning |
|---|---|
| `url` | Server URL, e.g. `http://plex.local:32400`. |
| `token` | `X-Plex-Token`. Find it via Plex's "Get Info → View XML" on any item. |
| `section` | Name of the music library section, usually `Music`. |
| `path_map` | Local prefix → the prefix Plex sees, e.g. `{ "/srv/music" = "/music" }` for a container. Longest match wins. Leave out if identical. |

On each push tapedeck triggers a scan of the section, waits for it to finish, matches
files by path, deletes any existing playlist with the same name and recreates it. The
Plex playlist is named after the playlist kind (`Weekly Jams`), so it is replaced each
week rather than piling up. The dated history stays in the m3u8 files.
