# tapedeck

**Your ListenBrainz recommendations, downloaded, tagged, filed and in Plex — automatically.**

[![CI](https://github.com/arunoruto/tapedeck/actions/workflows/ci.yml/badge.svg)](https://github.com/arunoruto/tapedeck/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/arunoruto/tapedeck?include_prereleases)](https://github.com/arunoruto/tapedeck/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

ListenBrainz builds you a fresh *Weekly Jams* and *Weekly Exploration* every week.
tapedeck turns them into music you actually own: it looks every track up on MusicBrainz,
downloads it with yt-dlp, writes proper tags, files it into a clean `Artist/Album/Track`
tree, keeps an m3u8 per playlist and pushes the playlist to Plex. Run it once, or let the
daemon do it every few hours.

```
 ListenBrainz ──▶ MusicBrainz ──▶ yt-dlp ──▶ tags ──▶ Artist/Album (Year)/01 - Title.opus
   playlists        metadata      audio      lofty           │
                                                             ├──▶ Playlists/*.m3u8
                                                             └──▶ Plex playlist
```

The binary is called `td`.

## Why

I used [Explo](https://github.com/LumePart/Explo) for this and wanted a few things it
doesn't do: a real library layout instead of a flat folder of files, a database so a song
that shows up in three weekly playlists is downloaded once, a daemon with a CLI that talks
to it, and output that scripts (or an AI agent) can parse. tapedeck is that, in Rust.

## Features

- **Library, not a dump.** Lidarr-style path templates:
  `{album_artist}/{album} ({year})/{track:02} - {title}.{ext}` by default, yours if you prefer.
- **Metadata from MusicBrainz**, keyed by the recording IDs ListenBrainz already gives you.
  Consistent artist/album/year/track number, and a hard guarantee against duplicates.
- **Tags written by tapedeck**, not by whatever the download site had. Album artist and
  track artist are kept apart, so featured guests don't split an album across folders.
- **An m3u8 for every fetched playlist**, dated like ListenBrainz names them. Your
  recommendation history, readable by any player, no tapedeck required.
- **Plex sync** that scans the library, matches files and recreates the playlist.
- **Daemon or one-shot, same CLI.** `td daemon` runs on an interval and listens on a Unix
  socket; every other `td` command talks to it if it's running and does the work itself if
  not.
- **`--json` everywhere.** One JSON object per line, for scripts and agents.
- **SQLite as the single source of truth**, with `token_file` support so secrets stay out
  of your config.
- **Nix first-class**: flake, binary cache, Home Manager module with a systemd user service.

## Install

**Prebuilt binaries** (Linux and macOS, x86_64 and arm64) are attached to each
[release](https://github.com/arunoruto/tapedeck/releases). With
[bin](https://github.com/marcosnils/bin):

```sh
bin install github.com/arunoruto/tapedeck
```

**Nix**, with x86_64-linux builds served from `arunoruto.cachix.org` (the flake advertises
the cache; accept it with `--accept-flake-config` or add it to your substituters):

```sh
nix run github:arunoruto/tapedeck -- --help
```

**Cargo**:

```sh
cargo install --git https://github.com/arunoruto/tapedeck
```

Binary and cargo installs need `yt-dlp` and `ffmpeg` on `PATH`. The Nix package wraps
them in.

## Quick start

```sh
mkdir -p ~/.config/tapedeck
curl -o ~/.config/tapedeck/config.toml \
  https://raw.githubusercontent.com/arunoruto/tapedeck/main/config.example.toml
$EDITOR ~/.config/tapedeck/config.toml     # library path, ListenBrainz user, Plex token
td sync --no-push                          # first run: download and file, skip Plex
td sync                                    # then with Plex
```

A minimal config:

```toml
library = "/srv/music"

[listenbrainz]
user = "you"
playlists = ["weekly-jams", "weekly-exploration"]

[sync]
targets = ["plex"]

[sync.plex]
url = "http://plex.local:32400"
token_file = "/run/secrets/plex-token"     # or token = "..."
section = "Music"
```

Everything else has a default. The full reference is in
[docs/configuration.md](docs/configuration.md).

## Commands

| Command | What it does |
|---|---|
| `td sync [--playlist KIND] [--no-push]` | Fetch playlists, download what's missing, tag, file, write m3u8, push |
| `td status` | Track counts by status (`wanted` / `ready` / `failed`) |
| `td search QUERY` | Search the library by artist, album or title |
| `td playlists` | Fetched playlists and where they've been synced |
| `td push PLAYLIST [--to TARGET]` | Re-push one playlist |
| `td retry` | Reset failed downloads so the next sync tries again |
| `td daemon` | Run as a service: listen on the socket, sync every `sync.interval` |

Add `--json` to any of them for newline-delimited JSON instead of text.

## Running as a service

With Home Manager, import `tapedeck.homeModules.default` from the flake and:

```nix
programs.tapedeck = {
  enable = true;
  settings = { library = "/srv/music"; listenbrainz.user = "you"; /* ... */ };
};
services.tapedeck.enable = true;
```

Without Nix, [`systemd/tapedeck.service`](systemd/tapedeck.service) is a user unit for a
cargo-installed `td`.

## What the library looks like

```
/srv/music/
├── Bicep/
│   └── Isles (2021)/
│       └── 01 - Apricots.opus
├── Bonobo/
│   └── Fragments (2022)/
│       └── 06 - Polyghost.opus
└── Playlists/
    ├── Weekly Jams for you, week of 2026-10-13 Mon.m3u8
    └── Weekly Exploration for you, week of 2026-10-13 Mon.m3u8
```

## Roadmap

Working today: the whole pipeline above, yt-dlp as the download backend, Plex as the sync
target. Next, roughly in order:

1. A TUI for browsing the library and playlists and watching downloads.
2. More sync targets (Jellyfin, plain m3u export) and download backends (Soulseek).
   Both are small traits; see [docs/architecture.md](docs/architecture.md#extension-points).
3. Playback, eventually.

## Development

```sh
nix develop                        # rust toolchain, yt-dlp, ffmpeg
cargo test                         # offline suite
cargo test -- --include-ignored    # also the live ListenBrainz / MusicBrainz tests
nix build                          # what CI and the cache build; runs the offline suite
```

Design notes live in [docs/architecture.md](docs/architecture.md). Releases are cut by
pushing a `v*` tag matching `Cargo.toml`; CI builds the binaries and publishes them.

## License

[MIT](LICENSE)
