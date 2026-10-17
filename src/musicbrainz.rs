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
    /// First credited artist of the recording.
    pub artist_mbid: Option<String>,
    pub album_artist: String,
    pub album_artist_mbid: String,
    pub album: String,
    pub album_mbid: String,
    pub year: Option<i64>,
    pub track_no: Option<i64>,
    pub length_ms: Option<i64>,
}

pub struct MusicBrainz {
    http: Client,
}

impl MusicBrainz {
    pub fn new(http: Client) -> Self {
        Self { http }
    }

    async fn get(&self, url: &str, what: &str) -> Result<Value> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let res = self.http.get(url).send().await?;
            // ponytail: MusicBrainz allows 1 req/s per client; a sleep after each call keeps us polite without a rate limiter.
            tokio::time::sleep(Duration::from_secs(1)).await;
            let status = res.status();
            if (status == 503 || status == 429) && attempt < 4 {
                tokio::time::sleep(Duration::from_secs(2 * attempt)).await;
                continue;
            }
            return Ok(res.error_for_status().with_context(|| format!("musicbrainz {what}"))?.json().await?);
        }
    }

    pub async fn recording(&self, mbid: &str) -> Result<Recording> {
        // ponytail: a lookup returns at most 25 releases; for very popular recordings the studio album
        // can fall outside that window. Switch to browsing /release?recording=<mbid>&limit=100 if it bites.
        let url = format!("{API}/recording/{mbid}?inc=artist-credits+releases+release-groups+media&fmt=json");
        let v = self.get(&url, &format!("recording {mbid}")).await?;
        parse_recording(&v, mbid)
    }

    /// Best recording MBID for an untagged file: search by artist and title, then prefer the
    /// hit whose length matches the file. None if nothing scores well enough.
    /// Lucene ties most hits at 100 in no stable order, so we ask for the maximum and let
    /// `pick_search_hit` sort it out; a popular song has dozens of recordings (live, remasters,
    /// compilation copies, covers).
    pub async fn search_recording(&self, artist: &str, title: &str, length_ms: Option<i64>) -> Result<Option<String>> {
        let q = format!("recording:\"{}\" AND artist:\"{}\"", lucene(title), lucene(artist));
        let url = format!("{API}/recording/?query={}&limit=100&fmt=json", urlencoding::encode(&q));
        let v = self.get(&url, &format!("search {artist} - {title}")).await?;
        Ok(pick_search_hit(&v, artist, length_ms))
    }
}

fn lucene(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn pick_search_hit(v: &Value, artist: &str, length_ms: Option<i64>) -> Option<String> {
    const OFF_KINDS: &[&str] = &["karaoke", "instrumental", "live", "cover", "tribute", "remix"];
    let wanted = artist.to_lowercase();
    let hits: Vec<&Value> = v["recordings"]
        .as_array()?
        .iter()
        .filter(|r| r["score"].as_i64().unwrap_or(0) >= 80)
        // Covers match the search too (the work links back to the original); keep the artist's own.
        .filter(|r| wanted.is_empty() || credit(&r["artist-credit"]).0.to_lowercase().contains(&wanted))
        .filter(|r| {
            let dis = r["disambiguation"].as_str().unwrap_or("").to_lowercase();
            !OFF_KINDS.iter().any(|k| dis.contains(k))
        })
        .collect();
    // A recording that is on a plain studio album first (not only compilations, remaster reissues
    // or live sets), then closest length in 3 s steps, then the one on the most releases. A popular
    // song has dozens of one-release compilation copies within a few seconds of the album take.
    let best = hits.iter().min_by_key(|r| {
        let drift = match (length_ms, r["length"].as_i64()) {
            (Some(want), Some(l)) => (l - want).abs() / 3000,
            _ => i64::MAX / 2,
        };
        let releases = r["releases"].as_array().cloned().unwrap_or_default();
        let on_album = releases.iter().any(|rel| {
            rel["release-group"]["primary-type"].as_str() == Some("Album")
                && rel["release-group"]["secondary-types"].as_array().is_none_or(|t| t.is_empty())
        });
        (!on_album, drift, -(releases.len() as i64))
    })?;
    best["id"].as_str().map(str::to_owned)
}

pub fn parse_recording(v: &Value, mbid: &str) -> Result<Recording> {
    let (artist, artist_mbid) = credit(&v["artist-credit"]);
    let releases = v["releases"].as_array().cloned().unwrap_or_default();
    let release = releases.iter().min_by_key(|r| release_rank(r, &artist_mbid)).cloned().unwrap_or(Value::Null);
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
        artist_mbid: if artist_mbid.is_empty() { None } else { Some(artist_mbid) },
        album_artist,
        album_artist_mbid: if album_artist_mbid.is_empty() { mbid.to_string() } else { album_artist_mbid },
        album: release["title"].as_str().unwrap_or("Unknown Album").to_string(),
        // No release at all: fall back to the recording id so the album row is still unique.
        album_mbid: release["id"].as_str().unwrap_or(mbid).to_string(),
        year,
        track_no,
        length_ms: v["length"].as_i64(),
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

/// Which release a recording gets filed under. Lower wins, compared left to right:
/// official > no secondary types (compilation, live, DJ-mix, soundtrack, ...) >
/// released by the track's own artist > album > EP > single > earliest date.
fn release_rank(r: &Value, track_artist_mbid: &str) -> (u8, u8, u8, u8, (u16, u8, u8)) {
    let official = if r["status"].as_str() == Some("Official") { 0 } else { 1 };
    let plain = match r["release-group"]["secondary-types"].as_array() {
        Some(t) if !t.is_empty() => 1,
        _ => 0,
    };
    let own = match r["artist-credit"][0]["artist"]["id"].as_str() {
        Some(id) if id == track_artist_mbid => 0,
        _ => 1,
    };
    let ptype = match r["release-group"]["primary-type"].as_str() {
        Some("Album") => 0,
        Some("EP") => 1,
        Some("Single") => 2,
        _ => 3,
    };
    (official, plain, own, ptype, date_key(r["date"].as_str().unwrap_or("")))
}

/// "1993-02-22" → (1993, 2, 22). Missing parts sort after known ones, so a precise date beats a
/// bare year in the same year, and no date at all sorts last.
fn date_key(date: &str) -> (u16, u8, u8) {
    let mut parts = date.split('-').map(|p| p.parse::<u16>().ok());
    let y = parts.next().flatten().unwrap_or(9999);
    let m = parts.next().flatten().unwrap_or(99) as u8;
    let d = parts.next().flatten().unwrap_or(99) as u8;
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn release(id: &str, status: &str, date: &str, ptype: &str, secondary: &[&str], artist: &str) -> Value {
        json!({
            "id": id, "title": id, "status": status, "date": date,
            "release-group": {"primary-type": ptype, "secondary-types": secondary},
            "artist-credit": [{"name": artist, "joinphrase": "", "artist": {"id": artist}}],
            "media": [{"tracks": [{"position": 7}]}]
        })
    }

    fn recording(releases: Vec<Value>) -> Value {
        json!({
            "title": "Creep", "length": 238000,
            "artist-credit": [{"name": "Radiohead", "joinphrase": "", "artist": {"id": "radiohead"}}],
            "releases": releases
        })
    }

    #[test]
    fn studio_album_beats_compilations_live_and_bare_years() {
        let v = recording(vec![
            release("va-comp", "Official", "1993", "Album", &["Compilation"], "va"),
            release("live", "Official", "1992-01-01", "Album", &["Live"], "radiohead"),
            release("bootleg", "Bootleg", "1990-01-01", "Album", &[], "radiohead"),
            release("single", "Official", "1992-09-21", "Single", &[], "radiohead"),
            release("pablo-honey", "Official", "1993-02-22", "Album", &[], "radiohead"),
            release("reissue", "Official", "2009-03-24", "Album", &[], "radiohead"),
        ]);
        let r = parse_recording(&v, "rec").unwrap();
        assert_eq!(r.album_mbid, "pablo-honey");
        assert_eq!((r.year, r.track_no, r.length_ms), (Some(1993), Some(7), Some(238000)));
        assert_eq!(r.artist_mbid.as_deref(), Some("radiohead"));
    }

    #[test]
    fn undated_release_sorts_last_and_year_only_after_full_date() {
        assert!(date_key("1993-02-22") < date_key("1993"));
        assert!(date_key("1993") < date_key("1994-01-01"));
        assert!(date_key("2020-05") < date_key(""));
        let v = recording(vec![
            release("undated", "Official", "", "Album", &[], "radiohead"),
            release("dated", "Official", "2024-07-12", "Album", &[], "radiohead"),
        ]);
        assert_eq!(parse_recording(&v, "rec").unwrap().album_mbid, "dated");
    }

    #[test]
    fn splits_track_and_album_artist() {
        let v = json!({
            "title": "Polyghost",
            "artist-credit": [
                {"name": "Bonobo", "joinphrase": " feat. ", "artist": {"id": "bonobo-id"}},
                {"name": "Nicole Miglis", "joinphrase": "", "artist": {"id": "nm-id"}}
            ],
            "releases": [release("Fragments", "Official", "2022-01-14", "Album", &[], "bonobo-id")]
        });
        let r = parse_recording(&v, "rec").unwrap();
        assert_eq!(r.artist, "Bonobo feat. Nicole Miglis");
        assert_eq!((r.album_artist.as_str(), r.album_artist_mbid.as_str()), ("bonobo-id", "bonobo-id"));
    }

    #[test]
    fn search_prefers_matching_length_then_most_releases_and_skips_karaoke() {
        let q = json!([{"name": "Queen", "artist": {"id": "q"}}]);
        let v = json!({"recordings": [
            {"id": "cover", "score": 100, "length": 238000, "artist-credit": [{"name": "Philharmonix"}],
             "releases": [{"release-group": {"primary-type": "Album"}}]},
            {"id": "live", "score": 100, "length": 410000, "artist-credit": q, "releases": [{}, {}]},
            {"id": "karaoke", "artist-credit": q, "score": 100, "length": 238500, "disambiguation": "karaoke version", "releases": [{}]},
            {"id": "studio", "artist-credit": q, "score": 95, "length": 238000, "releases": [{"release-group": {"primary-type": "Album"}}, {}]},
            {"id": "remaster", "artist-credit": q, "score": 95, "length": 238000, "releases": [{"release-group": {"primary-type": "Album", "secondary-types": ["Compilation"]}}, {}, {}, {}, {}]},
            {"id": "comp-copy", "artist-credit": q, "score": 95, "length": 240000, "releases": [{}]},
            {"id": "junk", "artist-credit": q, "score": 40, "length": 238000, "releases": []}
        ]});
        assert_eq!(
            pick_search_hit(&v, "Queen", Some(240000)).as_deref(),
            Some("studio"),
            "album take beats an exact-length compilation copy"
        );
        assert_eq!(
            pick_search_hit(&v, "queen", None).as_deref(),
            Some("studio"),
            "no length: studio album beats a bigger pile of compilations"
        );
        assert_eq!(pick_search_hit(&json!({"recordings": [{"id": "junk", "score": 40}]}), "", None), None);
        assert_eq!(lucene(r#"Say "Hi""#), r#"Say \"Hi\""#);
    }

    #[test]
    fn no_release_falls_back_to_recording_id() {
        let v = json!({"title": "Lost", "artist-credit": [{"name": "X", "artist": {"id": "x"}}]});
        let r = parse_recording(&v, "rec").unwrap();
        assert_eq!((r.album.as_str(), r.album_mbid.as_str(), r.album_artist.as_str()), ("Unknown Album", "rec", "X"));
        assert!(parse_recording(&json!({}), "rec").is_err());
    }
}
