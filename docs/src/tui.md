# The TUI

Plain `td` opens it. Left to right: artists, albums, songs; `]` switches to the playlists,
local and remote. Every action asks before it touches anything.

<p align="center"><img src="https://raw.githubusercontent.com/arunoruto/tapedeck/main/assets/tui.gif" alt="td browsing the library and the playlists" width="900"></p>

## Keys

| Key | Does |
|---|---|
| `Tab`, `[`, `]`, `1`, `2` | Switch tabs |
| `h j k l`, arrows | Move between panes and rows; `Enter` or `l` drills into the next column |
| `^D` `^U`, `^F` `^B`, `g` `G` | Half page, page, ends |
| `/` | Filter by artist, title or album; `^U` wipes it while typing |
| `i` | Track info: tags, what is on disk, ListenBrainz player and MusicBrainz links, source URL |
| `Enter` on a track | Play it and queue the rest of the list |
| `space`, `n` `N`, `,` `.`, `+` `-`, `x` | Pause, next/previous, seek 10 s, volume, stop |
| `S` | Full sync |
| `R` | Re-download the selected track from a fresh search |
| `s` | Pin a download URL for the selected track |
| `m` | Make the selected track another MusicBrainz recording (fix a wrong match) |
| `p`, `u` | Push the selected playlist to the targets, or remove it from them |
| `d` | Delete the selected track and its file, or a remote playlist |
| `r`, `q` | Reload, quit |

Everything that deletes, re-downloads, pushes or syncs asks `[y/N]` first.

## Playback

The TUI plays through mpv: `Enter` on a track plays it and queues the rest of the list (the
album in the Library tab, the playlist in the Playlists tab), `space` pauses, `n`/`N` skip,
`,`/`.` seek 10 s, `+`/`-` change the volume, `x` stops. A line above the help shows what is
playing with a position bar. In remote mode the audio is streamed from the daemon.

```toml
[player]
mpv_bin = "mpv"       # default; the Home Manager wrapper puts its mpvPackage on PATH
scrobble = true       # report plays to ListenBrainz
```

Plays are reported to ListenBrainz the way it asks: "playing now" when a track starts, a
listen after half the track or four minutes, whichever comes first, with the recording id
attached. The daemon does the submitting with its `listenbrainz.token`, so a laptop in remote
mode never needs the token; the `listen` scope covers it. Listens that cannot be delivered
(offline, ListenBrainz down) are queued in the database and sent on the next play or daemon
start. Without a token on the daemon nothing is sent and nothing is queued.


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
