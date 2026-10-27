# Playlists and Plex

One current m3u8 per playlist kind lives in `playlists/` (`weekly-jams.m3u8`,
`weekly-exploration.m3u8`, `daily-jams.m3u8`), replaced on every sync and mirroring the
playlist on Plex. The week it replaces is kept under `playlists/archive/<kind>/<period>.m3u8`,
ISO week for weekly kinds and the date for daily ones, never pushed unless you ask. Tracks that
drop out of a playlist are never deleted: they stay in the library, only the playlist changes.
Playlist folders and files are lowercase slugs; your library's own paths follow
`path_template` and the targets show `playlist_title`.

`td push` takes a fetched playlist or any m3u8 file (the archive, a backup, something
hand-written); a same-titled playlist on the target is backed up first and replaced.
`td unpush` removes a playlist from a target only, backup first. `td remote ls`, `rm`,
`backup` and `restore` manage what another tool left on the target.

## YouTube playlists

`td import <link>` takes a YouTube or YouTube Music playlist (any link with `list=`, album
lists `OLAK5uy_…` included). Each video is matched to a MusicBrainz recording the way a
request is, then downloaded through the normal path, YouTube Music's song first, length
checked. So a playlist of music videos still ends up as album audio without the intros and
skits. The playlist keeps YouTube's title and order, gets its own m3u8
(`youtube-<list id>.m3u8`) and is pushed like any other. Running it again later picks up
new entries.

```sh
td import "https://www.youtube.com/playlist?list=PL…"            # dry run: what matches, what is unsure
td import "https://www.youtube.com/playlist?list=PL…" --apply    # import the sure ones
td import "https://www.youtube.com/playlist?list=PL…" --apply --also 4,9   # and the top match of 4 and 9
```

An entry is sure when title, artist and version match its MusicBrainz recording and the
uploader is the artist, a label or Topic channel, or the playlist's own curator. Everything
else is listed with the reason. What an external library (Lidarr) already has is not
downloaded. Mixes (`RD…` lists, which YouTube makes up as you go) and private lists (liked
music, watch later; they need a signed-in account) are refused; unlisted playlists work.

## Plex

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

