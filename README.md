<a id="readme-top"></a>

<div align="center">
  <a href="https://github.com/arunoruto/tapedeck">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="assets/logo-dark.svg">
      <source media="(prefers-color-scheme: light)" srcset="assets/logo-light.svg">
      <img alt="tapedeck" src="assets/logo-light.svg" width="220">
    </picture>
  </a>

  <h3 align="center">tapedeck</h3>

  <p align="center">
    Your ListenBrainz recommendations, downloaded, tagged, filed and in Plex. Automatically.
    <br />
    <a href="docs/configuration.md"><strong>Configuration reference »</strong></a>
    <br />
    <br />
    <a href="https://github.com/arunoruto/tapedeck/releases">Releases</a>
    ·
    <a href="https://github.com/arunoruto/tapedeck/issues/new?labels=bug">Report a bug</a>
    ·
    <a href="https://github.com/arunoruto/tapedeck/issues/new?labels=enhancement">Request a feature</a>
  </p>

  [![CI](https://github.com/arunoruto/tapedeck/actions/workflows/ci.yml/badge.svg)](https://github.com/arunoruto/tapedeck/actions/workflows/ci.yml)
  [![Release](https://img.shields.io/github/v/release/arunoruto/tapedeck?include_prereleases)](https://github.com/arunoruto/tapedeck/releases)
  [![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
  [![Cachix](https://img.shields.io/badge/cachix-arunoruto-5277C3?logo=nixos&logoColor=white)](https://arunoruto.cachix.org)
</div>

<details>
  <summary>Table of contents</summary>
  <ol>
    <li><a href="#about">About</a></li>
    <li><a href="#getting-started">Getting started</a></li>
    <li><a href="#usage">Usage</a></li>
    <li><a href="#running-as-a-service">Running as a service</a></li>
    <li><a href="#roadmap">Roadmap</a></li>
    <li><a href="#contributing">Contributing</a></li>
    <li><a href="#license">License</a></li>
    <li><a href="#acknowledgments">Acknowledgments</a></li>
  </ol>
</details>

## About

ListenBrainz builds you a fresh *Weekly Jams* and *Weekly Exploration* every week.
tapedeck turns them into music you own. It looks every track up on MusicBrainz, downloads
it with yt-dlp, writes proper tags, files it into a clean `Artist/Album/Track` tree, keeps
an m3u8 per playlist and pushes the playlist to Plex. Run it once, or let the daemon do it
every few hours.

```
 ListenBrainz ──▶ MusicBrainz ──▶ yt-dlp ──▶ tags ──▶ Artist/Album (Year)/01 - Title.opus
   playlists        metadata      audio      lofty           │
                                                             ├──▶ playlists/*.m3u8
                                                             └──▶ Plex playlist
```

I used [Explo](https://github.com/LumePart/Explo) for this and wanted a few things it
doesn't do: a real library layout instead of a flat folder of files, a database so a song
that shows up in three weekly playlists is downloaded once, a daemon with a CLI that talks
to it, and output that scripts (or an AI agent) can parse. tapedeck is that, in Rust. The
binary is called `td`.

**What you get**

- **A library, not a dump.** Lidarr-style path templates:
  `{album_artist}/{album} ({year})/{track:02} - {title}.{ext}` by default, yours if you prefer.
- **Metadata from MusicBrainz**, keyed by the recording IDs ListenBrainz already gives you.
  Consistent artist, album, year and track number, and a hard guarantee against duplicates.
- **Tags written by tapedeck**, not by whatever the download site had. Album artist and
  track artist are kept apart, so featured guests don't split an album across folders.
- **An m3u8 per playlist**, kept in step with the playlist on Plex and readable by any player,
  with last week's copy archived. Tracks that drop out of a playlist stay in your library.
- **Downloads from YouTube Music's song results first**, matched by length against the
  MusicBrainz recording and checked again after the download, so you get the album audio
  rather than the music video with the two-minute intro.
- **Plex sync** that scans the library, matches files and recreates the playlist.
- **Use it from your laptop**: the daemon serves its commands and the audio over HTTP behind a
  token, so `td tui` on another machine browses and controls the NAS.
- **Song requests from Telegram**: send a link or "Artist - Title", get the proper recording
  filed and a yearly Requests playlist on Plex; it asks only when it is not sure.
- **Other libraries respected**: point it at Lidarr's folder and a recording that exists there
  is never downloaded again; playlists use the better copy, and own duplicates can be swapped out.
- **Daemon or one-shot, same CLI.** `td daemon` runs on an interval and listens on a Unix
  socket; every other `td` command talks to it if it's running and does the work itself if
  not.
- **`--json` everywhere.** One JSON object per line, for scripts and agents.
- **Secrets stay out of the config** with `token_file`, and SQLite is the single source of
  truth.
- **Nix first-class**: flake, binary cache, Home Manager module with a systemd user service.

### Built with

[Rust](https://www.rust-lang.org) · [clap](https://github.com/clap-rs/clap) ·
[rusqlite](https://github.com/rusqlite/rusqlite) · [lofty](https://github.com/Serial-ATA/lofty-rs) ·
[reqwest](https://github.com/seanmonstar/reqwest) · [tokio](https://tokio.rs) ·
[yt-dlp](https://github.com/yt-dlp/yt-dlp) · [MusicBrainz](https://musicbrainz.org) ·
[ListenBrainz](https://listenbrainz.org)

<p align="right">(<a href="#readme-top">back to top</a>)</p>

## Getting started

### Prerequisites

- A [ListenBrainz](https://listenbrainz.org) account with listening history, so it has
  recommendations to generate.
- `yt-dlp` and `ffmpeg` on `PATH`, unless you install through Nix, which bundles them.
- Optionally a Plex server and its token.

### Installation

**Prebuilt binaries** for Linux and macOS (x86_64 and arm64) are attached to each
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

### First sync

```sh
mkdir -p ~/.config/tapedeck
curl -o ~/.config/tapedeck/config.toml \
  https://raw.githubusercontent.com/arunoruto/tapedeck/main/config.example.toml
$EDITOR ~/.config/tapedeck/config.toml     # library path, ListenBrainz user, Plex token
td sync --no-push                          # download and file, skip Plex
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

<p align="right">(<a href="#readme-top">back to top</a>)</p>

## Usage

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
| `td import DIR [--apply] [--move] [--delete-duplicates]` | Bring an existing folder (say, Explo's downloads) into the library; dry run by default, `--apply` copies and leaves the source alone, `--move` moves |
| `td remote ls` / `td remote rm GLOB [--yes]` | List or bulk-delete playlists on a sync target; `rm` backs up first |
| `td remote backup [GLOB]` / `td remote restore FILE` | Save target playlists as m3u8 + JSON, or recreate one from such a file |
| `td request URL\|"Artist - Title" [--pick N] [--video]` | Request one song: identify, fetch, file, add to this year's Requests playlist |
| `td external scan` / `td external dedupe [--dry-run]` | Index read-only libraries (Lidarr) and replace own copies of recordings they hold |
| `td remote relink --from DIR` | Point backup entries under DIR at the library's copy of the same recording; run before deleting an imported folder |
| `td daemon` | Run as a service: listen on the socket, sync every `sync.interval` |
| `td` / `td tui` | Browse the library (artist › album › song) and playlists; `i` shows a track's tags, file quality and MusicBrainz/ListenBrainz/source URLs; sync, re-download, pin a source, fix a match, push, delete, each behind a `[y/N]`. Catppuccin out of the box, your own colours via a TOML theme |

Output is a table or plain lines; add `--json` to any command for newline-delimited JSON
events instead, which is what scripts and agents should parse.

### The TUI

Plain `td` opens it. Left to right: artists, albums, songs; `]` switches to the playlists,
local and remote. Every action asks before it touches anything.

<p align="center"><img src="assets/tui.gif" alt="td browsing the library and the playlists" width="900"></p>

Colours come from a theme: Catppuccin Mocha by default, Latte built in for light terminals,
and your own as a TOML file under `themes/` next to the config (see
[docs/configuration.md](docs/configuration.md#themes)). The recording above is made with
[vhs](https://github.com/charmbracelet/vhs) from `assets/demo.tape`.

After a sync the library looks like this:

```
/srv/music/
├── Bicep/
│   └── Isles (2021)/
│       └── 01 - Apricots.opus
├── Bonobo/
│   └── Fragments (2022)/
│       └── 06 - Polyghost.opus
└── playlists/
    ├── weekly-jams.m3u8
    ├── weekly-exploration.m3u8
    └── archive/
        └── weekly-jams/
            └── 2026-w41.m3u8
```

<p align="right">(<a href="#readme-top">back to top</a>)</p>

## Running as a service

On NixOS, import `tapedeck.nixosModules.default` from the flake and:

```nix
services.tapedeck = {
  enable = true;
  extraGroups = [ "media" ];
  settings = { library = "/srv/music"; listenbrainz.user = "you"; /* ... */ };
};
```

That runs the daemon as its own system user from boot, shares its database and socket with
the `tapedeck` group so `td tui` works for members, and takes secrets from the system's
sops-nix. Details and the migration from a user unit are in
[docs/configuration.md](docs/configuration.md#nixos).

With Home Manager, import `tapedeck.homeModules.default` from the flake and:

```nix
programs.tapedeck = {
  enable = true;
  settings = { library = "/srv/music"; listenbrainz.user = "you"; /* ... */ };
};
services.tapedeck.enable = true;
```

Without Nix, [`systemd/tapedeck.service`](systemd/tapedeck.service) is a user unit for a
cargo-installed `td`. Either way, `td status` from a shell talks to the running daemon over
its socket.

<p align="right">(<a href="#readme-top">back to top</a>)</p>

## Roadmap

Working today: the whole pipeline above, yt-dlp as the download backend, Plex as the sync
target.

- [x] A TUI for browsing the library and playlists
- [x] Actions in the TUI: sync, re-download, pin a source, push, delete, with confirmation
- [ ] More sync targets: Jellyfin, plain m3u export
- [ ] More download backends: Soulseek
- [ ] Playback, eventually

Sync targets and download backends are each one small trait; see
[docs/architecture.md](docs/architecture.md#extension-points). Open
[issues](https://github.com/arunoruto/tapedeck/issues) for the rest.

<p align="right">(<a href="#readme-top">back to top</a>)</p>

## Contributing

```sh
nix develop                        # rust toolchain, yt-dlp, ffmpeg
cargo test                         # offline suite
cargo test -- --include-ignored    # also the live ListenBrainz / MusicBrainz tests
nix build                          # what CI and the cache build; runs the offline suite
```

CI runs `cargo fmt --check`, `clippy -D warnings`, the full test suite and the Nix build.
Design notes and the reasoning behind the non-obvious choices are in
[docs/architecture.md](docs/architecture.md). Releases are cut by pushing a `v*` tag matching
`Cargo.toml`; CI builds the binaries and publishes them.

<p align="right">(<a href="#readme-top">back to top</a>)</p>

## License

Distributed under the MIT License. See [`LICENSE`](LICENSE).

## Acknowledgments

- [Explo](https://github.com/LumePart/Explo), which proved the idea and whose users I was one of.
- [MusicBrainz](https://musicbrainz.org) and [ListenBrainz](https://listenbrainz.org) for the
  open data this depends on.
- [yt-dlp](https://github.com/yt-dlp/yt-dlp) for doing the hard part.
- [Best-README-Template](https://github.com/othneildrew/Best-README-Template) for the layout.

<p align="right">(<a href="#readme-top">back to top</a>)</p>
