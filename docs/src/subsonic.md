# Subsonic clients

tapedeck's daemon speaks the Subsonic API, the protocol Navidrome and Airsonic use, so any
Subsonic app plays your library and your ListenBrainz playlists straight from tapedeck and
scrobbles through it. There is nothing to install besides the client: [cliamp](https://github.com/bjarneo/cliamp)
in a terminal, Symfonium, Tempo or DSub on Android, play:Sub on iOS, Feishin or Supersonic on
the desktop. Plex stays optional.

## Switch it on

The Subsonic endpoints share the HTTP API's port, so `[server] listen` is all it takes:

```toml
[server]
listen = "0.0.0.0:8687"
```

In the client, add a Navidrome or Subsonic server with the URL `http://<host>:8687` and any
username and password: without `[server.subsonic]` the daemon treats every client as
anonymous, which on a LAN or a Tailscale network is all you need. Anonymous clients get the
`[server] anonymous` scopes, by default `read`, `stream` and `listen`, which is exactly what a
player uses.

If the port faces the internet, require a sign-in:

```toml
[server.subsonic]
user = "mirza"
password_file = "/run/secrets/subsonic-password"   # or password = "…" for a quick test
```

Clients then authenticate the Subsonic way (a salted MD5 token or the password itself) and,
once signed in, may do everything; the anonymous scopes no longer matter for them. The admin
bearer token of the HTTP API passes too.

## What a client sees

- **Playlists**: every ListenBrainz playlist tapedeck has, the current ones under the title
  Plex shows (`Weekly Jams`), older editions under their ListenBrainz name, plus the
  Requests playlists from the Telegram bot.
- **Artists and albums**: the library grouped by MusicBrainz release and artist, so names
  follow MusicBrainz and ids survive renames. Search covers artists, albums and titles.
- **Cover art**: from the Cover Art Archive by release id, fetched once per size and kept in
  `covers/` next to the database.
- **Streaming**: the original files with seeking, own and [external](external.md) ones alike.
- **Lyrics**: synced through the OpenSubsonic `songLyrics` extension, plain through
  `getLyrics`, from what [lyrics](lyrics.md) has gathered.
- **Scrobbling**: the client's "now playing" and "scrobble" calls become listens on your
  ListenBrainz account through the daemon's token, with the same queue and retry as the TUI.

Favourites and ratings are accepted and dropped, since tapedeck keeps no stars; playlist
editing from the client is not offered, playlists come from ListenBrainz. Artist and album
info pages, lyrics, podcasts, bookmarks and the play queue answer empty, so clients that ask
for them after login carry on. Both the ID3 (`getArtists`) and the folder (`getIndexes`,
`getMusicDirectory`) browsing styles work, and responses are XML by default or JSON with
`f=json`, as the protocol says.

## cliamp

```sh
cliamp setup          # pick Navidrome, enter http://sado:8687 and any user and password
cliamp                # the tapedeck playlists are in the provider panel, N opens the browser
```

cliamp reports plays back to the server, so listening in cliamp scrobbles to ListenBrainz
without a plugin.

## For other projects

The protocol layer is its own crate, [`crates/subsonic-server`](https://github.com/arunoruto/tapedeck/tree/main/crates/subsonic-server):
you implement a `Library` trait (songs, playlists, files, cover art, scrobbles) and mount
the axum router it gives you. tapedeck's own implementation is `src/subsonic.rs`, about
two hundred lines, and the crate has no tapedeck types in it, so it can move to a
repository of its own when someone wants it.
