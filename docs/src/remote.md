# Remote use

The daemon can serve the same commands it takes on its socket over HTTP, so `td` and `td tui`
on a laptop can talk to the NAS. On the daemon:

```toml
[server]
listen = "0.0.0.0:8687"           # or one address; nothing is served without `listen`
anonymous = ["read", "stream", "listen"]      # default: what a client without a token may do
token_file = "/run/secrets/tapedeck/api-token" # optional: the admin token
```

On the laptop:

```toml
library = "/unused"               # still required; not touched in remote mode
[listenbrainz]
user = "you"
[remote]
url = "http://sado:8687"
# token_file = "…"                # only for admin commands
```

With `[remote] url` set, every command and the TUI's data go to that daemon; no local database
or socket is used. The API is `POST /cmd` (a `Command` as JSON in, `Event`s as NDJSON out, the
socket protocol) and `GET /stream/<track id>` (the track's file with Range support, own and
external files alike).

Two levels of access. Without a token a client gets the `anonymous` scopes: `read` (status,
search, the listings the TUI shows), `stream` (the audio), `listen` (scrobbles); `request`
(`td request`) and `sync` can be added to the list. Everything else (delete, retag, push,
unpush, import, refile, remote rm, external dedupe, …) needs the admin token and answers
`403` with a plain message otherwise; the TUI shows it in its status line. A server with no
token configured simply has no admin access over HTTP; `td` on the server itself, through the
socket, keeps full rights.

Plain HTTP, so keep the port to the LAN or Tailscale. Note that the default anonymous scopes
expose the library listing and the audio to anyone who can reach the port.

Players that speak Subsonic (cliamp, Symfonium, Feishin, …) use the same port; see
[Subsonic clients](subsonic.md).
