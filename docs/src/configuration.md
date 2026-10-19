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
| `theme` | `catppuccin-mocha` | TUI colours. A built-in name (`catppuccin-mocha`, `catppuccin-latte`) or the stem of a file in `themes/` next to `config.toml`. See [Themes](tui.md#themes). |
| `playlist_dir` | `<library>/playlists` | Where the m3u8 files go: one current file per playlist kind (`weekly-jams.m3u8`), superseded ones under `archive/<kind>/`, playlist backups under `backup/<target>/`. Lowercase slugs throughout. |
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
| `token_file` | none | Path to a file containing the token, instead of `token`. See [Secrets](#secrets). |
| `playlists` | `["weekly-jams", "weekly-exploration"]` | Which generated playlists to track, by ListenBrainz `source_patch` name. `daily-jams` is the other common one. |

## `[download]`

| Key | Default | Meaning |
|---|---|---|
| `backend` | `ytdlp` | Download backend. Only `ytdlp` exists today. |
| `format` | `opus` | Passed to `yt-dlp --audio-format`. |
| `ytdlp_bin` | `yt-dlp` | Executable name or path. |
| `ffmpeg_bin` | none | Passed to yt-dlp as `--ffmpeg-location`. Default: whatever yt-dlp finds on `PATH`. |

yt-dlp is searched with `ytsearch1:<artist> - <title>` unless a track has an explicit
`source_url` in the database. ffmpeg must be available for the audio extraction.

## `[sync]`

| Key | Default | Meaning |
|---|---|---|
| `interval` | `6h` | How often the daemon runs `sync`. Suffixes: `s`, `m`, `h`, `d`. |
| `on_start` | `true` | Sync as soon as the daemon starts, and after every restart (which includes a Plex push). `false` waits one interval first. |
| `targets` | `[]` | Enabled targets, in push order. Only `plex` exists today. |
| `playlist_title` | `{name}` | Playlist name on the targets. `{name}` is the kind in title case (`Weekly Jams`), `{kind}` the raw kind (`weekly-jams`). Use something like `{name} (tapedeck)` if another tool already owns the plain names. |

### `[sync.plex]`

| Key | Meaning |
|---|---|
| `url` | Server URL, e.g. `http://plex.local:32400`. |
| `token` | `X-Plex-Token`. Find it via Plex's "Get Info → View XML" on any item. |
| `token_file` | Path to a file containing the token, instead of `token`. One of the two is required. |
| `section` | Name of the music library section, usually `Music`. |
| `path_map` | Local prefix → the prefix Plex sees, e.g. `{ "/srv/music" = "/music" }` for a container. Longest match wins. Leave out if identical. |

## `[steam]`

Game soundtracks bought on Steam, imported as they are; see [Steam soundtracks](steam.md).
`path` is the Steam client's `steamapps/music`; `username` a steamcmd account (logged in
once by hand) whose owned soundtracks are downloaded. Either or both. `steamcmd_bin`
(default `steamcmd`), `staging` (default `steam/` next to the database) and `interval`
(default `24h`, `"0"` off) tune the download side.

```toml
[steam]
path = "/home/you/.local/share/Steam/steamapps/music"
username = "you"
```

## `[server.subsonic]`

Sign-in for [Subsonic clients](subsonic.md) on `/rest/`, served on the `[server] listen` port.
Without `user`, every Subsonic client is anonymous and gets the `anonymous` scopes (enough on
a LAN). With `user` and `password` or `password_file`, clients must sign in and then may do
everything.

```toml
[server.subsonic]
user = "mirza"
password_file = "/run/secrets/subsonic-password"
```

## Environment variables

Every key can also come from the environment, which is how a container gets its paths and
secrets without a mounted file. The name is `TAPEDECK_` followed by the key path with `__`
(two underscores) between levels, in any case:

```sh
TAPEDECK_LIBRARY=/music
TAPEDECK_LISTENBRAINZ__USER=you
TAPEDECK_LISTENBRAINZ__PLAYLISTS='["weekly-jams", "daily-jams"]'
TAPEDECK_SYNC__TARGETS='["plex"]'
TAPEDECK_SYNC__PLEX__URL=http://plex:32400
TAPEDECK_SYNC__PLEX__SECTION=Music
TAPEDECK_SYNC__PLEX__TOKEN_FILE=/run/secrets/plex-token
TAPEDECK_SERVER__LISTEN=0.0.0.0:8687
```

Environment wins over the file. `true`/`false` and values starting with `[`, `{` or `"` are
read as TOML (arrays, inline tables, quoted strings); everything else is taken as a string, so
paths, URLs and durations need no quotes. An empty value removes the key, which switches off a
default the container image sets (`TAPEDECK_SERVER__LISTEN=`). When at least one `TAPEDECK_*`
variable is set, the config file may be missing altogether; `TAPEDECK_CONFIG` itself names the
file and is not a key.

Secrets work the same way: `TAPEDECK_LISTENBRAINZ__TOKEN=…` or, better, a `TOKEN_FILE` pointing
at a mounted secret, so the token never shows up in `docker inspect`.

## Secrets

Every `token` has a `token_file` twin. tapedeck reads the file when it is about to talk to
that service (the daemon at start, an in-process command when it needs the target; never for
commands that go through a running daemon's socket), strips the
trailing newline, and from then on behaves as if `token` had been set. Setting both is an
error. This is how you keep tokens out of a config that lives in Git or in the Nix store:
point `token_file` at whatever sops-nix, agenix or `systemd-creds` puts on disk.

The sections for `[player]`, `[server]`/`[remote]`, `[telegram]` and `[external]` are on their
own pages: [The TUI](tui.md#playback), [Remote use](remote.md), [Song requests](requests.md)
and [External libraries](external.md). The NixOS and Home Manager modules are under
[Installation](installation.md).
