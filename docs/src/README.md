# tapedeck <span class="stripes"></span>

<div class="hero">

ListenBrainz builds you a fresh *Weekly Jams* and *Weekly Exploration* every week.
tapedeck turns them into music you own. It looks every track up on MusicBrainz, downloads it with yt-dlp, writes proper tags, files it into a clean `Artist/Album/Track` tree, keeps an m3u8 per playlist and pushes the playlist to Plex. Run it once, or let the daemon do it every few hours. 

</div>

> **Vibe coded, built for one person.** tapedeck is written with AI assistance for my own
> setup and comes with no guarantees. You are welcome to try it and to
> [file issues](https://github.com/arunoruto/tapedeck/issues); I will try to fix them.

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
- **Plays, too**: `Enter` in the TUI plays a track and queues the rest through mpv, locally or
  streamed from the NAS, and scrobbles to ListenBrainz so next week's playlists know.
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

Where to go next: [Installation](installation.md), then [Quick start](quick-start.md). The
reference has every [configuration key](configuration.md) and every [command](commands.md).
The code is at [github.com/arunoruto/tapedeck](https://github.com/arunoruto/tapedeck).
