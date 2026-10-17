# tapedeck

Pulls your ListenBrainz recommendation playlists (Weekly Jams, Weekly Exploration, ...),
downloads the tracks, tags them from MusicBrainz, files them into a tidy library,
writes an m3u8 per playlist and pushes the playlist to Plex.

It started as a replacement for [Explo](https://github.com/LumePart/Explo) with the
parts I missed: a real library layout instead of a flat dump, a database so nothing is
downloaded twice, a daemon, and a CLI that can be scripted.

The binary is `td`.

## Status

Early. The sync pipeline works end to end: ListenBrainz → MusicBrainz → yt-dlp → tags →
library → m3u8 → Plex. Planned next, roughly in order: a TUI for browsing the library and
playlists, more sync targets (Jellyfin, plain m3u export), more download backends, and
eventually playback.

## Install

Prebuilt binaries for Linux and macOS (x86_64 and arm64) are attached to each
[release](https://github.com/arunoruto/tapedeck/releases). With
[bin](https://github.com/marcosnils/bin):

```sh
bin install github.com/arunoruto/tapedeck
```

With Nix (x86_64-linux builds are cached at `arunoruto.cachix.org`; the flake's `nixConfig`
offers the cache, accept it with `--accept-flake-config` or add it to your substituters):

```sh
nix run github:arunoruto/tapedeck -- --help
```

There is also a Home Manager module (`homeModules.default`) that writes the config and can
run the daemon as a user service, and an overlay. See
[docs/configuration.md](docs/configuration.md#home-manager).

With cargo:

```sh
cargo install --git https://github.com/arunoruto/tapedeck
```

The binary release and cargo install need `yt-dlp` and `ffmpeg` on `PATH`; the Nix
package wraps them in.

## Quick start

```sh
mkdir -p ~/.config/tapedeck
cp config.example.toml ~/.config/tapedeck/config.toml   # edit library path, LB user, Plex token
td sync
```

`td sync` fetches the playlists, downloads what is missing, writes
`<library>/Playlists/<playlist>.m3u8` and pushes to the configured targets.

## Commands

```
td sync [--playlist weekly-jams] [--no-push]   fetch, download, file, push
td status                                       track counts by status
td search <query>                               search artist / album / title
td playlists                                    fetched playlists and where they went
td push <playlist> [--to plex]                  re-push one playlist
td retry                                        reset failed downloads
td daemon                                       run as a service
```

Every command takes `--json` and then prints one JSON object per line, which is what to
use from scripts or an agent.

## Running as a service

`td daemon` listens on a Unix socket and runs `sync` every `sync.interval`. Any other
`td` command connects to the daemon when it is running and otherwise just does the work
itself, so the CLI behaves the same with or without the service.

A user unit for cargo installs is in [`systemd/tapedeck.service`](systemd/tapedeck.service);
Nix users get one from the Home Manager module with `services.tapedeck.enable`.

## Docs

- [Configuration](docs/configuration.md)
- [Architecture and design notes](docs/architecture.md)

## Development

```sh
nix develop                        # rust toolchain, yt-dlp, ffmpeg
cargo test                         # offline suite
cargo test -- --include-ignored    # also the live ListenBrainz / MusicBrainz tests
nix build                          # what CI and the cache build; runs the offline suite
```

Releases are cut by pushing a `v*` tag that matches the version in `Cargo.toml`; CI builds
the binaries and attaches them to the GitHub release.

## Library layout

Default template, overridable in the config:

```
Music/
  Artist/
    Album (2024)/
      01 - Title.opus
  Playlists/
    Weekly Jams for you, week of 2026-10-13 Mon.m3u8
```

## License

MIT
