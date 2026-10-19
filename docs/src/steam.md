# Steam soundtracks

Steam sells game soundtracks as "Music" apps: plain MP3 or FLAC files, tagged, often with
the artwork embedded, no DRM. MusicBrainz knows few of them, so tapedeck treats Steam as a
source of its own: the files come in as they are, tags trusted and kept, nothing retagged,
and from then on they are ordinary tracks in the library, in Plex, in the TUI and for
[Subsonic clients](subsonic.md), with the game's artwork as the cover.

## Two ways in

**The Steam client's folder.** If a machine with Steam installed the soundtracks, point
tapedeck at `steamapps/music`; every subfolder is one soundtrack:

```toml
[steam]
path = "/home/you/.local/share/Steam/steamapps/music"
```

**The account, through steamcmd.** Without a Steam client, tapedeck can list what the
account owns, pick out the soundtracks and download new ones itself:

```toml
[steam]
username = "you"
# steamcmd_bin = "steamcmd"      # or a full path; on NixOS "${pkgs.steamcmd}/bin/steamcmd" (unfree)
# staging = "/var/lib/tapedeck/steam"   # downloads land here before import; the default
# interval = "24h"               # how often the daemon looks; "0" turns it off
```

steamcmd must be logged in once by hand, because Steam Guard asks for a code; after that
its cached session works unattended. Do it as the user tapedeck runs as, with the same
`HOME`:

```sh
sudo -u tapedeck HOME=/var/lib/tapedeck steamcmd +login you +quit
```

No Web API key is needed: steamcmd already knows which licenses the account holds, and the
store answers what kind of app each one is. The first pass asks the store once per owned
app, slowly enough to stay under its rate limit, and remembers the answers, so a library
with a few hundred games takes a few minutes the first time and seconds after that.

Both can be set at once.

## Commands

```sh
td steam list      # the folders under `path`, the owned soundtracks, what is imported
td steam sync      # import new soundtracks (downloading first when `username` is set)
```

The daemon runs `sync` on `interval`. Imports copy; the Steam folder is never touched, and a
staged download is removed once it is in the library.

## How they are filed

- The album is the folder, with tags deciding the name and the album artist (the value most
  files agree on, so one odd tag does not split an album); an untagged file gets its title
  and track number from the file name and the folder as artist and album.
- Files are copied into the usual `path_template` place, with their tags as they are.
- Ids are synthetic (`steam:<app id or folder>`), so `td retag` and the MusicBrainz
  renaming logic leave these rows alone, and Subsonic clients get the embedded picture or a
  `cover.jpg` next to the files as the cover.
- They do not join ListenBrainz playlists, and are not pushed anywhere as playlists; they
  show up as albums wherever the library is browsed.
