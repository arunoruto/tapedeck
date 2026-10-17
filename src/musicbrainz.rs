//! Recording lookups. One request per new track, cached forever in SQLite by the caller.

use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;

const API: &str = "https://musicbrainz.org/ws/2";

#[derive(Debug, Clone, PartialEq)]
pub struct Recording {
    pub title: String,
    pub artist: String,
    pub album_artist: String,
    pub album_artist_mbid: String,
    pub album: String,
    pub album_mbid: String,
    pub year: Option<i64>,
    pub track_no: Option<i64>,
}

pub struct MusicBrainz {
    http: Client,
}

impl MusicBrainz {
    pub fn new(http: Client) -> Self {
        Self { http }
    }

    pub async fn recording(&self, mbid: &str) -> Result<Recording> {
        let url = format!("{API}/recording/{mbid}?inc=artist-credits+releases+release-groups+media&fmt=json");
        let mut attempt = 0;
        let v: Value = loop {
            attempt += 1;
            let res = self.http.get(&url).send().await?;
            // ponytail: MusicBrainz allows 1 req/s per client; a sleep after each call keeps us polite without a rate limiter.
            tokio::time::sleep(Duration::from_secs(1)).await;
            let status = res.status();
            if (status == 503 || status == 429) && attempt < 4 {
                tokio::time::sleep(Duration::from_secs(2 * attempt)).await;
                continue;
            }
            break res.error_for_status().with_context(|| format!("musicbrainz recording {mbid}"))?.json().await?;
        };
        parse_recording(&v, mbid)
    }
}

pub fn parse_recording(v: &Value, mbid: &str) -> Result<Recording> {
    let (artist, artist_mbid) = credit(&v["artist-credit"]);
    let releases = v["releases"].as_array().cloned().unwrap_or_default();
    let release = releases.iter().min_by_key(|r| release_rank(r)).cloned().unwrap_or(Value::Null);
    let (album_artist, album_artist_mbid) = match &release["artist-credit"] {
        Value::Array(a) if !a.is_empty() => credit(&release["artist-credit"]),
        _ => (artist.clone(), artist_mbid.clone()),
    };
    let year = release["date"].as_str().and_then(|d| d.get(..4)).and_then(|y| y.parse().ok());
    let track_no = release["media"]
        .as_array()
        .and_then(|m| m.first())
        .and_then(|m| m["tracks"].as_array())
        .and_then(|t| t.first())
        .and_then(|t| t["position"].as_i64());

    Ok(Recording {
        title: v["title"].as_str().context("recording has no title")?.to_string(),
        artist,
        album_artist,
        album_artist_mbid: if album_artist_mbid.is_empty() { mbid.to_string() } else { album_artist_mbid },
        album: release["title"].as_str().unwrap_or("Unknown Album").to_string(),
        // No release at all: fall back to the recording id so the album row is still unique.
        album_mbid: release["id"].as_str().unwrap_or(mbid).to_string(),
        year,
        track_no,
    })
}

/// Joined artist credit ("A feat. B") and the first artist's MBID.
fn credit(v: &Value) -> (String, String) {
    let credits = v.as_array().cloned().unwrap_or_default();
    let name = credits
        .iter()
        .map(|c| format!("{}{}", c["name"].as_str().unwrap_or(""), c["joinphrase"].as_str().unwrap_or("")))
        .collect::<String>();
    let id = credits.first().and_then(|c| c["artist"]["id"].as_str()).unwrap_or("").to_string();
    (name, id)
}

/// Lower is better: official albums first, then by date, so reissues and compilations lose.
fn release_rank(r: &Value) -> (u8, u8, String) {
    let official = if r["status"].as_str() == Some("Official") { 0 } else { 1 };
    let album = match r["release-group"]["primary-type"].as_str() {
        Some("Album") => 0,
        Some("EP") => 1,
        Some("Single") => 2,
        _ => 3,
    };
    (official, album, r["date"].as_str().unwrap_or("9999").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn picks_official_album_and_splits_artists() {
        let v = json!({
            "title": "Polyghost",
            "artist-credit": [
                {"name": "Bonobo", "joinphrase": " feat. ", "artist": {"id": "bonobo-id"}},
                {"name": "Nicole Miglis", "joinphrase": "", "artist": {"id": "nm-id"}}
            ],
            "releases": [
                {"id": "comp", "title": "Chill Hits 2023", "status": "Official", "date": "2023-01-01",
                 "release-group": {"primary-type": "Album"},
                 "artist-credit": [{"name": "Various Artists", "joinphrase": "", "artist": {"id": "va"}}],
                 "media": [{"tracks": [{"position": 9}]}]},
                {"id": "album", "title": "Fragments", "status": "Official", "date": "2022-01-14",
                 "release-group": {"primary-type": "Album"},
                 "artist-credit": [{"name": "Bonobo", "joinphrase": "", "artist": {"id": "bonobo-id"}}],
                 "media": [{"tracks": [{"position": 1}]}]},
                {"id": "single", "title": "Polyghost", "status": "Official", "date": "2021-11-01",
                 "release-group": {"primary-type": "Single"}}
            ]
        });
        let r = parse_recording(&v, "rec").unwrap();
        assert_eq!(r.artist, "Bonobo feat. Nicole Miglis");
        assert_eq!(r.album_artist, "Bonobo");
        assert_eq!(r.album_artist_mbid, "bonobo-id");
        assert_eq!(
            (r.album.as_str(), r.album_mbid.as_str(), r.year, r.track_no),
            ("Fragments", "album", Some(2022), Some(1))
        );
    }

    #[test]
    fn no_release_falls_back_to_recording_id() {
        let v = json!({"title": "Lost", "artist-credit": [{"name": "X", "artist": {"id": "x"}}]});
        let r = parse_recording(&v, "rec").unwrap();
        assert_eq!((r.album.as_str(), r.album_mbid.as_str(), r.album_artist.as_str()), ("Unknown Album", "rec", "X"));
        assert!(parse_recording(&json!({}), "rec").is_err());
    }
}
