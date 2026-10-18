# Installation

## Prerequisites

- A [ListenBrainz](https://listenbrainz.org) account with listening history, so it has
  recommendations to generate.
- `yt-dlp` and `ffmpeg` on `PATH`, unless you install through Nix, which bundles them.
- Optionally a Plex server and its token.


## Install the binary

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


## Container

Images for amd64 and arm64 are on the GitHub registry, built from the same Nix package as
the flake (`td`, yt-dlp, ffmpeg, no mpv). `latest` always points at the newest release;
each release is also tagged with its version (`0.1.0`), and manual builds from main appear
as `sha-<short sha>` only, so `latest` never moves to an untested commit.

```sh
docker run -d --name tapedeck \
  -v /srv/music:/music -v tapedeck-data:/data -p 8687:8687 \
  -e TAPEDECK_LIBRARY=/music -e TAPEDECK_LISTENBRAINZ__USER=you \
  -e TAPEDECK_SYNC__TARGETS='["plex"]' -e TAPEDECK_SYNC__PLEX__URL=http://plex:32400 -e TAPEDECK_SYNC__PLEX__SECTION=Music \
  -e TAPEDECK_SYNC__PLEX__TOKEN_FILE=/run/secrets/plex-token -v ./plex-token:/run/secrets/plex-token:ro \
  ghcr.io/arunoruto/tapedeck:latest
docker exec -it tapedeck td status        # any command goes through the daemon's socket
```

Or as a `compose.yaml`, with the secret as a file next to it:

```yaml
services:
  tapedeck:
    image: ghcr.io/arunoruto/tapedeck:latest
    container_name: tapedeck
    restart: unless-stopped
    ports:
      - "8687:8687"
    volumes:
      - /srv/music:/music
      - tapedeck-data:/data
      # - ./config.toml:/config.toml:ro     # instead of, or on top of, the variables below
      # - ./themes:/themes:ro
    environment:
      TAPEDECK_LIBRARY: /music
      TAPEDECK_LISTENBRAINZ__USER: you
      TAPEDECK_LISTENBRAINZ__PLAYLISTS: '["weekly-jams", "weekly-exploration", "daily-jams"]'
      TAPEDECK_SYNC__TARGETS: '["plex"]'
      TAPEDECK_SYNC__PLEX__URL: http://plex:32400
      TAPEDECK_SYNC__PLEX__SECTION: Music
      TAPEDECK_SYNC__PLEX__TOKEN_FILE: /run/secrets/plex-token
    secrets:
      - plex-token

secrets:
  plex-token:
    file: ./plex-token

volumes:
  tapedeck-data:
```

```sh
docker compose up -d
docker compose exec -it tapedeck td status
```

For podman with systemd, a [Quadlet](https://docs.podman.io/en/latest/markdown/podman-systemd.unit.5.html)
in `~/.config/containers/systemd/tapedeck.container` (rootless) or
`/etc/containers/systemd/tapedeck.container` (system) does the same. The token goes into a
podman secret first, so it is never part of the unit:

```sh
podman secret create plex-token ./plex-token
```

```ini
[Unit]
Description=tapedeck music sync daemon
After=network-online.target
Wants=network-online.target

[Container]
Image=ghcr.io/arunoruto/tapedeck:latest
ContainerName=tapedeck
# Follows new releases when podman-auto-update.timer is enabled; latest only ever moves to a release.
AutoUpdate=registry
PublishPort=8687:8687
Volume=/srv/music:/music
Volume=tapedeck-data:/data
# Volume=%h/.config/tapedeck/config.toml:/config.toml:ro   # instead of, or on top of, the variables
Environment=TAPEDECK_LIBRARY=/music
Environment=TAPEDECK_LISTENBRAINZ__USER=you
Environment=TAPEDECK_LISTENBRAINZ__PLAYLISTS=["weekly-jams", "weekly-exploration", "daily-jams"]
Environment=TAPEDECK_SYNC__TARGETS=["plex"]
Environment=TAPEDECK_SYNC__PLEX__URL=http://plex:32400
Environment=TAPEDECK_SYNC__PLEX__SECTION=Music
Environment=TAPEDECK_SYNC__PLEX__TOKEN_FILE=/run/secrets/plex-token
Secret=plex-token,type=mount

[Service]
Restart=always

[Install]
WantedBy=default.target
```

```sh
systemctl --user daemon-reload && systemctl --user start tapedeck   # drop --user for a system unit
journalctl --user -u tapedeck -f
podman exec -it tapedeck td status
```

For a rootless unit that should run without you logged in, `loginctl enable-linger $USER`;
for a system unit use `WantedBy=multi-user.target`.

On NixOS the same container is a few lines of `virtualisation.oci-containers`, with the token
coming straight from sops-nix (the [native module](#nixos) below is the better fit on NixOS,
but this keeps tapedeck next to other containers you already run this way):

```nix
virtualisation.oci-containers = {
  backend = "podman";
  containers.tapedeck = {
    image = "ghcr.io/arunoruto/tapedeck:latest";
    ports = [ "8687:8687" ];
    volumes = [
      "/srv/music:/music"
      "tapedeck-data:/data"
      "${config.sops.secrets."tapedeck/plex-token".path}:/run/secrets/plex-token:ro"
      # "/etc/tapedeck/config.toml:/config.toml:ro"   # instead of, or on top of, the variables
    ];
    environment = {
      TAPEDECK_LIBRARY = "/music";
      TAPEDECK_LISTENBRAINZ__USER = "you";
      TAPEDECK_LISTENBRAINZ__PLAYLISTS = ''["weekly-jams", "weekly-exploration", "daily-jams"]'';
      TAPEDECK_SYNC__TARGETS = ''["plex"]'';
      TAPEDECK_SYNC__PLEX__URL = "http://plex:32400";
      TAPEDECK_SYNC__PLEX__SECTION = "Music";
      TAPEDECK_SYNC__PLEX__TOKEN_FILE = "/run/secrets/plex-token";
    };
    extraOptions = [ "--label=io.containers.autoupdate=registry" ];
  };
};
```

That becomes `podman-tapedeck.service`; `journalctl -u podman-tapedeck -f` and
`podman exec -it tapedeck td status` work as above.

The image runs `td daemon`. Configuration comes from [environment variables](configuration.md#environment-variables),
from a file mounted at `/config.toml`, or both (environment wins); themes go next to the
file, in `/themes`. The database and the socket live under `/data`, so give that a volume.
The HTTP API listens on 8687 by default (Lidarr's port plus one), so `td` on another machine
and the TUI's playback work with `remote.url = "http://<host>:8687"`; set
`TAPEDECK_SERVER__LISTEN=` (empty) to switch it off, or `TAPEDECK_SERVER__TOKEN_FILE` to
require a token for admin commands. The container runs as root unless you pass
`--user <uid>:<gid>` matching the owner of the library.

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


## Without Nix, as a service

[`systemd/tapedeck.service`](https://github.com/arunoruto/tapedeck/blob/main/systemd/tapedeck.service)
is a user unit for a cargo-installed `td`. Either way, `td status` from a shell talks to the
running daemon over its socket.
