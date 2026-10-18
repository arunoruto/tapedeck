# Quick start

## First sync

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
[Configuration](configuration.md).

## What a sync does

Every run fetches the newest playlist of each configured kind from ListenBrainz, resolves
new recordings on MusicBrainz, downloads what is missing (YouTube Music first), tags and
files the audio, writes one m3u8 per playlist kind and pushes each playlist to the sync
targets. Running it twice downloads nothing new. `td daemon` does the same on a timer and
answers the other commands over a socket; see [Playlists](playlists.md) for what ends up
where and [Commands](commands.md) for the rest.

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
