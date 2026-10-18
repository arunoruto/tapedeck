//! The flat song list grouped into albums and artists, and the JSON shapes of each.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::{Playlist, Song};

// ponytail: regrouped on every request; cache behind a library version if tens of thousands
// of songs make browsing slow.
pub(crate) struct Shelf {
    pub songs: Vec<Song>,
}

pub(crate) struct Album<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub artist: &'a str,
    pub artist_id: &'a str,
    pub year: Option<i64>,
    pub cover_art: Option<&'a str>,
    pub music_brainz_id: Option<&'a str>,
    /// Position of the newest song in the library's order, a stand-in for "added recently".
    pub newest: usize,
    pub songs: Vec<&'a Song>,
}

pub(crate) struct Artist<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub music_brainz_id: Option<&'a str>,
    pub albums: Vec<Album<'a>>,
}

impl Shelf {
    pub fn albums(&self) -> Vec<Album<'_>> {
        let mut by: BTreeMap<&str, Album<'_>> = BTreeMap::new();
        for (i, s) in self.songs.iter().enumerate() {
            let a = by.entry(&s.album_id).or_insert_with(|| Album {
                id: &s.album_id,
                name: &s.album,
                artist: &s.album_artist,
                artist_id: &s.artist_id,
                year: s.year,
                cover_art: s.cover_art.as_deref(),
                music_brainz_id: s.album_music_brainz_id.as_deref(),
                newest: i,
                songs: vec![],
            });
            a.newest = i;
            a.year = a.year.or(s.year);
            a.cover_art = a.cover_art.or(s.cover_art.as_deref());
            a.songs.push(s);
        }
        let mut albums: Vec<_> = by.into_values().collect();
        for a in &mut albums {
            a.songs.sort_by_key(|s| (s.track.unwrap_or(i64::MAX), s.title.to_lowercase()));
        }
        albums.sort_by_key(|a| a.name.to_lowercase());
        albums
    }

    pub fn artists(&self) -> Vec<Artist<'_>> {
        let mut by: BTreeMap<&str, Artist<'_>> = BTreeMap::new();
        for a in self.albums() {
            let mbid = a.songs.first().and_then(|s| s.artist_music_brainz_id.as_deref());
            let row = by.entry(a.artist_id).or_insert_with(|| Artist {
                id: a.artist_id,
                name: a.artist,
                music_brainz_id: mbid,
                albums: vec![],
            });
            row.albums.push(a);
        }
        let mut artists: Vec<_> = by.into_values().collect();
        artists.sort_by_key(|a| a.name.to_lowercase());
        artists
    }
}

/// `getArtists` groups by first letter; digits and symbols land under `#`.
pub(crate) fn index(artists: &[Artist<'_>]) -> Value {
    let mut by: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for a in artists {
        let first = a.name.chars().next().filter(|c| c.is_alphabetic()).map(|c| c.to_uppercase().to_string());
        by.entry(first.unwrap_or_else(|| "#".into())).or_default().push(artist_json(a));
    }
    json!(by.into_iter().map(|(name, artist)| json!({ "name": name, "artist": artist })).collect::<Vec<_>>())
}

pub(crate) fn artist_json(a: &Artist<'_>) -> Value {
    json!({
        "id": a.id,
        "name": a.name,
        "albumCount": a.albums.len(),
        "coverArt": a.albums.iter().find_map(|al| al.cover_art),
        "musicBrainzId": a.music_brainz_id,
    })
}

pub(crate) fn album_json(a: &Album<'_>) -> Value {
    json!({
        "id": a.id,
        "parent": a.artist_id,
        "isDir": true,
        "name": a.name,
        "album": a.name,
        "title": a.name,
        "artist": a.artist,
        "artistId": a.artist_id,
        "coverArt": a.cover_art,
        "songCount": a.songs.len(),
        "duration": a.songs.iter().map(|s| s.duration.unwrap_or(0)).sum::<i64>(),
        "year": a.year,
        "musicBrainzId": a.music_brainz_id,
    })
}

pub(crate) fn content_type(path: &str) -> (&'static str, String) {
    let suffix = path.rsplit_once('.').map(|(_, e)| e.to_lowercase()).unwrap_or_default();
    let ct = match suffix.as_str() {
        "opus" | "ogg" | "oga" => "audio/ogg",
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "m4a" | "aac" | "mp4" => "audio/mp4",
        "wav" => "audio/wav",
        _ => "application/octet-stream",
    };
    (ct, suffix)
}

pub(crate) fn song_json(s: &Song) -> Value {
    let (ct, suffix) = content_type(&s.path);
    json!({
        "id": s.id,
        "parent": s.album_id,
        "isDir": false,
        "isVideo": false,
        "type": "music",
        "title": s.title,
        "album": s.album,
        "artist": s.artist,
        "albumArtist": s.album_artist,
        "track": s.track,
        "year": s.year,
        "coverArt": s.cover_art,
        "duration": s.duration,
        "contentType": ct,
        "suffix": suffix,
        "path": s.path,
        "albumId": s.album_id,
        "artistId": s.artist_id,
        "musicBrainzId": s.music_brainz_id,
    })
}

pub(crate) fn playlist_json(p: &Playlist, owner: &str) -> Value {
    json!({
        "id": p.id,
        "name": p.name,
        "comment": p.comment,
        "owner": p.owner.as_deref().unwrap_or(owner),
        "public": true,
        "songCount": p.song_count,
        "duration": p.duration,
        "created": p.created,
        "changed": p.changed,
        "coverArt": p.cover_art,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::strip_nulls;

    #[test]
    fn groups_by_album_and_artist_ids() {
        let s = |id: &str,
                 title: &str,
                 album: &str,
                 album_id: &str,
                 artist: &str,
                 artist_id: &str,
                 no,
                 art: Option<&str>| Song {
            id: id.into(),
            title: title.into(),
            album: album.into(),
            album_id: album_id.into(),
            artist: artist.into(),
            album_artist: artist.into(),
            artist_id: artist_id.into(),
            track: Some(no),
            path: format!("{title}.opus"),
            duration: Some(60),
            cover_art: art.map(str::to_string),
            ..Default::default()
        };
        let shelf = Shelf {
            songs: vec![
                s("1", "Two", "Album A", "al-a", "Zed", "ar-z", 2, Some("al-a")),
                s("2", "One", "Album A", "al-a", "Zed", "ar-z", 1, Some("al-a")),
                s("3", "Solo", "Album B", "al-b", "Zed", "ar-z", 1, None),
                s("4", "Loose", "Video", "al-v", "99 Problems", "ar-9", 1, None),
            ],
        };
        let albums = shelf.albums();
        assert_eq!(albums.iter().map(|a| a.name).collect::<Vec<_>>(), ["Album A", "Album B", "Video"]);
        assert_eq!(albums[0].songs.iter().map(|s| s.title.as_str()).collect::<Vec<_>>(), ["One", "Two"]);
        assert_eq!(albums[0].newest, 1);
        let artists = shelf.artists();
        assert_eq!(artists.iter().map(|a| a.name).collect::<Vec<_>>(), ["99 Problems", "Zed"]);
        assert_eq!(artists[1].albums.len(), 2);
        let idx = index(&artists);
        assert_eq!(idx[0]["name"], "#");
        assert_eq!(idx[1]["name"], "Z");
        let song = song_json(&shelf.songs[0]);
        assert_eq!(song["contentType"], "audio/ogg");
        assert_eq!(song["coverArt"], "al-a");
        let mut loose = song_json(&shelf.songs[3]);
        strip_nulls(&mut loose);
        assert!(loose.get("coverArt").is_none(), "no art, no key: {loose}");
    }
}
