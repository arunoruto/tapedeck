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
| `theme` | `catppuccin-mocha` | TUI colours. A built-in name (`catppuccin-mocha`, `catppuccin-latte`) or the stem of a file in `themes/` next to `config.toml`. See [Themes](#themes). |
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

## Secrets

Every `token` has a `token_file` twin. tapedeck reads the file when it is about to talk to
that service (the daemon at start, an in-process command when it needs the target; never for
commands that go through a running daemon's socket), strips the
trailing newline, and from then on behaves as if `token` had been set. Setting both is an
error. This is how you keep tokens out of a config that lives in Git or in the Nix store:
point `token_file` at whatever sops-nix, agenix or `systemd-creds` puts on disk.

## NixOS

On a server tapedeck should run like Plex or Lidarr do: as its own system user from boot,
secrets from the system's sops-nix, files shared with the media group, logs in
`journalctl -u tapedeck`. The flake exports a NixOS module for that. Add the flake as an input,
import `tapedeck.nixosModules.default`, then:

```nix
services.tapedeck = {
  enable = true;
  extraGroups = [ "media" ];           # the group that owns the library
  settings = {
    library = "/srv/music/tapedeck";
    listenbrainz = { user = "you"; playlists = [ "weekly-jams" "weekly-exploration" "daily-jams" ]; };
    sync.targets = [ "plex" ];
    sync.plex = {
      url = "http://127.0.0.1:32400";
      token_file = config.sops.secrets."tapedeck/plex-token".path;
      section = "Music";
    };
  };
};
sops.secrets."tapedeck/plex-token".owner = config.services.tapedeck.user;
users.users.you.extraGroups = [ "tapedeck" ];   # to use td / td tui against the daemon
```

The token file can stay readable by the service user alone (sops' default mode 0400): `td`
reads it only when it talks to a target itself, and with the daemon running every command
from a shell goes through the socket instead. Group membership and `TAPEDECK_CONFIG` apply to
new login sessions only, so log out and in once after the first switch; until then `td` falls
back to `~/.config/tapedeck/config.toml`.

What the module does:

- Runs `td daemon --config /etc/tapedeck/config.toml` as user and group `tapedeck` (both
  created; override `user`/`group` to use existing ones), with `extraGroups` as supplementary
  groups. `package`, `ytdlpPackage` and `ffmpegPackage` work as in the Home Manager module
  (no mpv on a server).
- `database` defaults to `/var/lib/tapedeck/tapedeck.db` and `socket` to
  `/run/tapedeck/tapedeck.sock`; both directories are mode 0770 and the unit's umask is 0002,
  so members of the group can run `td status` or `td tui` and have their actions go through
  the daemon. Downloaded files come out group-writable as well, which is what Plex and Samba
  want.
- Puts `td` in `environment.systemPackages` and sets `TAPEDECK_CONFIG` to the system config,
  so every shell uses the same config, database and socket.
- Restarts on failure and on a changed config, waits for `network-online.target`, and is
  sandboxed (`ProtectSystem=strict` with the library and playlist folder writable, no home,
  private tmp and devices, no new privileges).

Make the library writable by the service before the first start. The `2` in the mode is the
setgid bit: new files and folders then inherit the `media` group instead of the daemon's own,
so Samba users in that group can edit them (the daemon's umask already makes them
group-writable). Alternatively set `group = "media"` on the module to run with that group as
the primary one.

```sh
sudo install -d -o tapedeck -g media -m 2775 /srv/music/tapedeck
# an existing library:
sudo find /srv/music/tapedeck -type d -exec chmod g+s {} +
```

External libraries (`[external] paths`) need no extra permissions: the service only reads them,
and `ProtectSystem=strict` allows that everywhere.

Moving from the Home Manager daemon on the same machine: disable `services.tapedeck` in Home
Manager and switch, then

```sh
sudo install -d -o tapedeck -g tapedeck -m 770 /var/lib/tapedeck
sudo mv ~/.local/share/tapedeck/tapedeck.db /var/lib/tapedeck/
sudo chown tapedeck:tapedeck /var/lib/tapedeck/tapedeck.db
sudo chown -R tapedeck:media /srv/music/tapedeck && sudo chmod -R g+rwX /srv/music/tapedeck
```

and switch to the NixOS configuration. The playlist folder inside the library and its backups
come along with the library. Set `sync.on_start = false` if you would rather the first sync
waited for the interval than ran at boot.

## Home Manager

The flake exports a Home Manager module. Add the flake as an input, import
`tapedeck.homeModules.default`, then:

```nix
programs.tapedeck = {
  enable = true;
  settings = {
    library = "/srv/music";
    listenbrainz.user = "me";
    sync.targets = [ "plex" ];
    sync.plex = {
      url = "http://plex.local:32400";
      token_file = config.sops.secrets.plex-token.path;
      section = "Music";
    };
  };
};
services.tapedeck.enable = true;   # systemd user unit running `td daemon`
```

`settings` is written verbatim to `~/.config/tapedeck/config.toml`, so every key in this
document applies. The package defaults to the flake's own build; `programs.tapedeck.package`
overrides it.

yt-dlp and ffmpeg come from *your* nixpkgs (`programs.tapedeck.ytdlpPackage`,
`programs.tapedeck.ffmpegPackage`, defaulting to `pkgs.yt-dlp` and `pkgs.ffmpeg-headless`),
not from the flake's pin, so they are shared with the rest of your system instead of
duplicated. The flake publishes `tapedeck-unwrapped` (the Rust build, what the cache holds)
and `tapedeck` (a thin wrapper that puts the two tools on `PATH`); the module re-wraps, so
changing the tools never rebuilds the Rust part.

Two things to know about the user service on a headless machine:

- It only runs while that user has a session, unless the NixOS side sets
  `users.users.<name>.linger = true`.
- If the token comes from Home Manager's sops-nix, the unit is ordered after
  `sops-nix.service` automatically so the secret exists when `td` starts.

## Plex sync behaviour

The library (as Plex sees it, after `path_map`) has to be inside one of the folders the
section scans, or Plex will never index the files. tapedeck checks this before pushing and
fails with the section's folder list if it isn't; add the library as another folder of
the section in Plex's settings.

On each push tapedeck triggers a scan of the section, waits for it to finish, matches
files by path, deletes any existing playlist with the same name and recreates it. The
Plex playlist is named after the playlist kind (`Weekly Jams`), so it is replaced each
week rather than piling up. The m3u8 on disk (`playlists/weekly-jams.m3u8`) follows the same
rule; the week it replaces is kept under `playlists/archive/weekly-jams/2026-w41.m3u8`. Tracks
that leave a playlist stay in the library.

## Playback: `[player]`

The TUI plays through mpv: `Enter` on a track plays it and queues the rest of the list (the
album in the Library tab, the playlist in the Playlists tab), `space` pauses, `n`/`N` skip,
`,`/`.` seek 10 s, `+`/`-` change the volume, `x` stops. A line above the help shows what is
playing with a position bar. In remote mode the audio is streamed from the daemon.

```toml
[player]
mpv_bin = "mpv"       # default; the Home Manager wrapper puts its mpvPackage on PATH
```

## Remote use: `[server]` and `[remote]`

The daemon can serve the same commands it takes on its socket over HTTP, so `td` and `td tui`
on a laptop can talk to the NAS. On the daemon:

```toml
[server]
listen = "0.0.0.0:7878"           # or one address; nothing is served without `listen`
anonymous = ["read", "stream", "listen"]      # default: what a client without a token may do
token_file = "/run/secrets/tapedeck/api-token" # optional: the admin token
```

On the laptop:

```toml
library = "/unused"               # still required; not touched in remote mode
[listenbrainz]
user = "you"
[remote]
url = "http://sado:7878"
# token_file = "…"                # only for admin commands
```

With `[remote] url` set, every command and the TUI's data go to that daemon; no local database
or socket is used. The API is `POST /cmd` (a `Command` as JSON in, `Event`s as NDJSON out, the
socket protocol) and `GET /stream/<track id>` (the track's file with Range support, own and
external files alike).

Two levels of access. Without a token a client gets the `anonymous` scopes: `read` (status,
search, the listings the TUI shows), `stream` (the audio), `listen` (scrobbles); `request`
(`td request`) and `sync` can be added to the list. Everything else (delete, retag, push,
unpush, import, refile, remote rm, external dedupe, …) needs the admin token and answers
`403` with a plain message otherwise; the TUI shows it in its status line. A server with no
token configured simply has no admin access over HTTP; `td` on the server itself, through the
socket, keeps full rights.

Plain HTTP, so keep the port to the LAN or Tailscale. Note that the default anonymous scopes
expose the library listing and the audio to anyone who can reach the port.

## Song requests (Telegram)

Songs you hear elsewhere can be sent to tapedeck as a link or as "Artist - Title", from a
Telegram chat or with `td request`. tapedeck works out the recording on MusicBrainz, fetches
the best audio for it (YouTube Music first, the link itself as the fallback), files and tags it
like any other track, and puts it on this year's **Requests** playlist (`requests-2026`, so
`playlists/requests-2026.m3u8` and "Requests 2026" on Plex, newest first; a new one starts
each January and the old one stays).

```toml
[telegram]
token_file = "/run/secrets/tapedeck/telegram-token"   # or token = "…"
allowed_users = [123456789]                            # Telegram user ids; everyone else is ignored
```

The bot runs inside the daemon with long polling (no inbound port) and answers only the
listed users. It asks only when it is not sure: no MusicBrainz match, a title or artist that
does not line up, a remix or live version the recording lacks, an upload by someone who is not
the artist (the classic no-name cover). Then it offers up to three candidates, "Exactly this
video" (download the link as sent, tagged from its own metadata) and Cancel as buttons. A
confident answer comes back as "✅ Added: Artist – Title (Album, 3:07)" with "👍 Correct",
"👎 Wrong song" and Undo buttons; a 👍 or 👎 reaction on that message, or a `/wrong` reply,
does the same as the buttons. Confirming settles the message (the buttons go) and is
remembered; `td status` shows how many requests are still unconfirmed. Open questions live in memory, so a daemon restart forgets them; send the link
again. On the command line, `td request URL` does the same and, when unsure, prints the
candidates with numbers for `td request URL --pick N` or `td request URL --video`.

## External libraries

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

## Themes

The TUI takes its colours from a theme. Two are built in, `catppuccin-mocha` (default) and
`catppuccin-latte` for light terminals. To make your own, drop a TOML file into `themes/` next
to `config.toml` (so `~/.config/tapedeck/themes/mine.toml` by default) and set
`theme = "mine"`. A user file with a built-in's name replaces it.

Every key is required; colours are `"#rrggbb"` or terminal names such as `"blue"`,
`"lightred"`, `"gray"`. The built-in files in the repository's `themes/` folder are the
reference; this is Mocha:

```toml
name = "Catppuccin Mocha"

text = "#cdd6f4"          # normal text
muted = "#6c7086"         # help line, secondary text, remote playlists
accent = "#89b4fa"        # focused pane border, active tab, key hints
border = "#45475a"        # unfocused pane borders
title = "#b4befe"         # pane titles
selection_bg = "#313244"  # highlighted row
selection_fg = "#f5e0dc"
ready = "#a6e3a1"         # status colours
failed = "#f38ba8"
wanted = "#f9e2af"
warning = "#fab387"       # confirmations, status messages
stripes = ["#f38ba8", "#fab387", "#f9e2af"]   # the cassette stripes, any number of colours
```

The terminal's own background is left alone, so a theme only needs to pick colours that read
on it. With Home Manager, `xdg.configFile."tapedeck/themes/mine.toml".source = ./mine.toml;`
puts a theme next to the generated config.
