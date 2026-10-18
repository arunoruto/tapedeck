# Migrating from Explo

If a folder of downloads already exists, tapedeck can take it over without touching it.

1. **Back up the playlists Explo made on Plex**: `td remote backup --to plex` writes one m3u8
   plus a JSON sidecar per playlist under `playlists/backup/plex/`.
2. **Dry run the import**: `td import /path/to/explo`. Every file is identified by its
   MusicBrainz id when tagged, otherwise by a search on artist, title and length; copies of the
   same recording are grouped, the one closest to the recording's length is kept, and a
   recording more than 20 s off is a miss rather than a wrong cut.
3. **Apply**: `td import /path/to/explo --apply` copies the kept files into the library and
   leaves the source alone (`--move` moves instead; `--delete-duplicates` only with `--move`).
   Backups are rewritten to point at the library copies.
4. **Relink** what still points into the old folder: `td remote relink --from /path/to/explo`.
5. **Fix the odd match** with `td retag ID --recording MBID`, found in the TUI with `i`.
6. **Delete the old folder** yourself, then `td remote rm "Weekly-Jams-*" --yes` for Explo's
   playlists on Plex (backed up first).

`td refile` re-resolves every track against MusicBrainz, retags and moves files whose path
changed; `--dry-run` shows the moves, including artist folders that MusicBrainz renamed
(`td artist NAME --folder F` pins a name you'd rather keep).
