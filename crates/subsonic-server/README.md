# subsonic-server

Serve any music library to Subsonic clients. You implement one trait, `Library`, that
answers with songs, playlists, files, cover art and scrobbles; this crate speaks the
[Subsonic](https://www.subsonic.org/pages/api.jsp) /
[OpenSubsonic](https://opensubsonic.netlify.app/) protocol to cliamp, Symfonium, Feishin,
DSub, play:Sub, Tempo and the rest, as an [axum](https://github.com/tokio-rs/axum) router you
mount wherever you like.

> **Vibe coded.** This crate grew inside [tapedeck](https://github.com/arunoruto/tapedeck),
> written with AI assistance for the author's own use. It works with the clients the author
> uses and comes with no guarantees. Try it, and file issues; the author will try to fix them.

## What it does

- Request parsing for GET and form POST, `.view` suffixes, repeated parameters.
- Authentication the Subsonic way: `u` + `t`/`s` (salted MD5) or `u` + `p` (plain or `enc:`),
  with the proper error codes (10, 40, 42). Credentials are optional: with none configured
  every client is anonymous, and your `Library` decides what anonymous clients may do.
- The response envelope as XML (the protocol's default), JSON (`f=json`) and JSONP, with
  `type`, `serverVersion` and `openSubsonic` set, and nulls stripped so Gson-based clients
  do not crash.
- Browsing in both styles: ID3 (`getArtists`, `getArtist`, `getAlbum`, `getSong`,
  `getAlbumList2`) and folder (`getIndexes`, `getMusicDirectory`), grouped from the flat
  song list you return, so your ids stay yours.
- Lyrics: `getLyricsBySongId` (the OpenSubsonic `songLyrics` extension, synced lines parsed
  from LRC text) and the classic `getLyrics`, when your `Library` returns any.
- `search2`/`search3`, `getRandomSongs`, `getPlaylists`/`getPlaylist`, `stream`/`download`
  with Range requests, `getCoverArt`, `scrobble` (now playing and submissions), `getUser`,
  `getLicense`, `getScanStatus`, `getOpenSubsonicExtensions`.
- Empty but valid answers for the calls clients fire after login that a plain library has
  nothing for (artist/album info, top and similar songs, lyrics, podcasts, radio, shares,
  bookmarks, play queue), and accepted no-ops for stars and ratings.

Not covered: writing playlists, stars that persist, transcoding, XML for anything beyond the
response envelope's generic mapping, API-key authentication (clients get error 42 and fall
back to passwords).

## Use

```rust
use std::{path::PathBuf, sync::Arc};
use subsonic_server::{Error, Library, Playlist, Scope, Server, Song};

struct MyLibrary;

#[async_trait::async_trait]
impl Library for MyLibrary {
    async fn songs(&self) -> Result<Vec<Song>, Error> { Ok(vec![]) }
    async fn song(&self, id: &str) -> Result<Option<Song>, Error> { Ok(None) }
    async fn playlists(&self) -> Result<Vec<Playlist>, Error> { Ok(vec![]) }
    async fn playlist_songs(&self, id: &str) -> Result<Option<Vec<Song>>, Error> { Ok(None) }
    async fn file(&self, id: &str) -> Result<Option<PathBuf>, Error> { Ok(None) }
    // cover, scrobble and anonymous_may have defaults: no art, scrobbles dropped, everything allowed.
}

let router: axum::Router = Server::new(MyLibrary)
    .name("myserver", env!("CARGO_PKG_VERSION"))
    .credentials("me", "sesame")      // leave out for an open LAN server
    .router();                         // serves /rest/{method}
```

Mount the router with `Router::merge` next to your own routes. If your application has its
own authentication, insert `subsonic_server::Trusted(true)` into the request extensions from
your middleware and the request passes as signed in.

## License

MIT.
