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

Every `token` has a `token_file` twin. tapedeck reads the file at startup, strips the
trailing newline, and from then on behaves as if `token` had been set. Setting both is an
error. This is how you keep tokens out of a config that lives in Git or in the Nix store:
point `token_file` at whatever sops-nix, agenix or `systemd-creds` puts on disk.

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
week rather than piling up. The dated history stays in the m3u8 files.
