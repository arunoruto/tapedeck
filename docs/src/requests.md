# Song requests

Songs you hear elsewhere can be sent to tapedeck as a link or as "Artist - Title", from a
Telegram chat or with `td request`. tapedeck works out the recording on MusicBrainz, fetches
the best audio for it (YouTube Music first, the link itself as the fallback), files and tags it
like any other track, and puts it on this year's **Requests** playlist (`requests-2026`, so
`playlists/requests-2026.m3u8` and "Requests 2026" on Plex, newest first; a new one starts
each January and the old one stays).

```toml
[telegram]
token_file = "/run/secrets/tapedeck/telegram-token"   # or token = "…"
allowed_users = [123456789]                            # Telegram user ids; everyone else is ignored
```

The bot runs inside the daemon with long polling (no inbound port) and answers only the
listed users. It asks only when it is not sure: no MusicBrainz match, a title or artist that
does not line up, a remix or live version the recording lacks, an upload by someone who is not
the artist (the classic no-name cover). Then it offers up to three candidates, "Exactly this
video" (download the link as sent, tagged from its own metadata) and Cancel as buttons. A
confident answer comes back as "✅ Added: Artist – Title (Album, 3:07)" with "👍 Correct",
"👎 Wrong song" and Undo buttons; a 👍 or 👎 reaction on that message, or a `/wrong` reply,
does the same as the buttons. Confirming settles the message (the buttons go) and is
remembered; `td status` shows how many requests are still unconfirmed. Open questions live in memory, so a daemon restart forgets them; send the link
again. On the command line, `td request URL` does the same and, when unsure, prints the
candidates with numbers for `td request URL --pick N` or `td request URL --video`.

