//! Recording lookups. One request per new track, cached forever in SQLite by the caller.

use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Client;
use serde::Serialize;
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
            // 503 is also how MusicBrainz says "slow down"; the search endpoint does it a lot.
            // Back off 2, 4, 8, 16, 32 s (or what Retry-After says) before giving up.
            if (status == 503 || status == 429) && attempt < 6 {
                let retry_after = res.headers().get("retry-after").and_then(|v| v.to_str().ok()?.parse().ok());
                tokio::time::sleep(Duration::from_secs(retry_after.unwrap_or(1u64 << attempt).min(60))).await;
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
        let title = strip_feat(title);
        let v = self.search(artist, &title).await?;
        Ok(pick_search_hit(&v, artist, &title, length_ms))
    }

    /// Ranked candidates for a human to choose from (no length cap; the caller judges).
    pub async fn search_candidates(&self, artist: &str, title: &str, length_ms: Option<i64>) -> Result<Vec<Candidate>> {
        let title = strip_feat(title);
        let v = self.search(artist, &title).await?;
        Ok(search_candidates_from(&v, artist, &title, length_ms))
    }

    async fn search(&self, artist: &str, title: &str) -> Result<Value> {
        let q = if artist.is_empty() {
            format!("recording:\"{}\"", lucene(title))
        } else {
            format!("recording:\"{}\" AND artist:\"{}\"", lucene(title), lucene(artist))
        };
        let url = format!("{API}/recording/?query={}&limit=100&fmt=json", urlencoding::encode(&q));
        self.get(&url, &format!("search {artist} - {title}")).await
    }
}

fn lucene(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// "APT. (feat. Bruno Mars)" → "APT."; MusicBrainz keeps guests in the artist credit, not the title.
pub fn strip_feat(title: &str) -> String {
    let lower = title.to_lowercase();
    let cut = ["(feat.", "(feat ", "[feat.", "[feat ", "(ft.", "[ft.", " feat. ", " feat ", " ft. "]
        .iter()
        .filter_map(|m| lower.find(m))
        .min();
    match cut {
        Some(p) => title[..p].trim().to_string(),
        None => title.to_string(),
    }
}

/// Words that mark a different take ("Counting Stars (Voicedude rmx)"). A hit may only carry
/// them if the query title does too.
const VERSION_WORDS: &[&str] = &[
    "remix",
    "rmx",
    "mix",
    "edit",
    "intro",
    "outro",
    "clap",
    "dirty",
    "clean",
    "extended",
    "radio",
    "club",
    "dub",
    "bootleg",
    "rework",
    "reprise",
    "slowed",
    "sped",
    "live",
    "version",
    "instrumental",
    "acoustic",
    "demo",
    "karaoke",
    "mashup",
    "cover",
    "stripped",
    "cappella",
    "acappella",
    "unplugged",
    "orchestral",
    "symphonic",
    "piano",
    "reverb",
    "nightcore",
];

/// Not a thing anyone bought: DJ-pool dumps and MusicBrainz's placeholders.
fn real_release(rel: &Value) -> bool {
    let title = rel["title"].as_str().unwrap_or("").to_lowercase();
    rel["status"].as_str() != Some("Pseudo-Release") && title != "unclustered files" && !title.starts_with("[non-album")
}

/// Lowercase, feat. stripped, punctuation collapsed: "Old Town Road" == "old town road".
fn norm(title: &str) -> String {
    strip_feat(title)
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn version_words(title: &str) -> Vec<&'static str> {
    let lower = title.to_lowercase();
    VERSION_WORDS.iter().copied().filter(|w| lower.split(|c: char| !c.is_alphanumeric()).any(|tok| tok == *w)).collect()
}

/// A search hit, enough to show someone and ask.
#[derive(Debug, Clone, Serialize)]
pub struct Candidate {
    pub mbid: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub length_ms: Option<i64>,
}

fn pick_search_hit(v: &Value, artist: &str, title: &str, length_ms: Option<i64>) -> Option<String> {
    let best = rank_search_hits(v, artist, title, length_ms).into_iter().next()?;
    // A different cut is not a match, however well the name fits: a 4:07 radio edit must not
    // become the 6:09 album take. 20 s, the same rule the downloader applies.
    if let (Some(want), Some(got)) = (length_ms, best["length"].as_i64())
        && (want - got).abs() > 20_000
    {
        return None;
    }
    best["id"].as_str().map(str::to_owned)
}

/// The filtered, ranked hits as candidates (best first), for flows that ask a human.
fn search_candidates_from(v: &Value, artist: &str, title: &str, length_ms: Option<i64>) -> Vec<Candidate> {
    rank_search_hits(v, artist, title, length_ms)
        .into_iter()
        .take(5)
        .filter_map(|r| {
            let album = r["releases"]
                .as_array()
                .and_then(|rs| rs.iter().find(|rel| real_release(rel)).or(rs.first()))
                .and_then(|rel| rel["title"].as_str())
                .unwrap_or("")
                .to_string();
            Some(Candidate {
                mbid: r["id"].as_str()?.to_string(),
                title: r["title"].as_str().unwrap_or("").to_string(),
                artist: credit(&r["artist-credit"]).0,
                album,
                length_ms: r["length"].as_i64(),
            })
        })
        .collect()
}

fn rank_search_hits<'a>(v: &'a Value, artist: &str, title: &str, length_ms: Option<i64>) -> Vec<&'a Value> {
    const OFF_KINDS: &[&str] = &["karaoke", "instrumental", "live", "cover", "tribute", "remix"];
    let wanted = artist.to_lowercase();
    let allowed = version_words(title);
    let Some(all) = v["recordings"].as_array() else { return vec![] };
    let mut hits: Vec<&Value> = all
        .iter()
        .filter(|r| r["score"].as_i64().unwrap_or(0) >= 80)
        // Covers match the search too (the work links back to the original); keep the artist's own.
        .filter(|r| wanted.is_empty() || credit(&r["artist-credit"]).0.to_lowercase().contains(&wanted))
        .filter(|r| version_words(r["title"].as_str().unwrap_or("")).iter().all(|w| allowed.contains(w)))
        .filter(|r| {
            let dis = r["disambiguation"].as_str().unwrap_or("").to_lowercase();
            !OFF_KINDS.iter().any(|k| dis.contains(k))
        })
        .collect();
    // Order: a title that is exactly the query (no extra "(intro edit)" tail); real releases at
    // all (DJ-pool pseudo-releases and "Unclustered Files" are not); on an official plain album,
    // single or EP (not only compilations, DJ mixes or live sets); then closest length in 3 s
    // steps; then the one on the most releases. A popular song has dozens of one-release
    // compilation copies within a few seconds of the album take.
    let want_title = norm(title);
    hits.sort_by_cached_key(|r| {
        let drift = match (length_ms, r["length"].as_i64()) {
            (Some(want), Some(l)) => (l - want).abs() / 3000,
            _ => i64::MAX / 2,
        };
        let releases: Vec<&Value> =
            r["releases"].as_array().map(|v| v.iter().filter(|rel| real_release(rel)).collect()).unwrap_or_default();
        let on_official = releases.iter().any(|rel| {
            rel["status"].as_str() == Some("Official")
                && matches!(rel["release-group"]["primary-type"].as_str(), Some("Album" | "Single" | "EP"))
                && rel["release-group"]["secondary-types"].as_array().is_none_or(|t| t.is_empty())
        });
        let exact = norm(r["title"].as_str().unwrap_or("")) == want_title;
        (!exact, releases.is_empty(), !on_official, drift, -(releases.len() as i64))
    });
    hits
}

pub fn parse_recording(v: &Value, mbid: &str) -> Result<Recording> {
    let (artist, artist_mbid) = credit(&v["artist-credit"]);
    let releases = v["releases"].as_array().cloned().unwrap_or_default();
    let release = releases.iter().min_by_key(|r| release_rank(r, &artist_mbid)).cloned().unwrap_or(Value::Null);
    let (album_artist, album_artist_mbid) = match &release["artist-credit"] {
        Value::Array(a) if !a.is_empty() => canonical_credit(&release["artist-credit"]),
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
    credit_with(v, |c| c["name"].as_str())
}

/// Same, but with each artist's current entity name instead of the credited spelling, so a
/// Japanese edition credited to "ザ・チェインスモーカーズ" still files under "The Chainsmokers" and a
/// renamed band (Eskimo Callboy → Electric Callboy) gets one folder. When the entity name is not
/// what someone wants on disk (Miley Cyrus became "MILEY"), `td artist … --folder` pins a name.
fn canonical_credit(v: &Value) -> (String, String) {
    credit_with(v, |c| c["artist"]["name"].as_str().or(c["name"].as_str()))
}

fn credit_with(v: &Value, name: impl Fn(&Value) -> Option<&str>) -> (String, String) {
    let credits = v.as_array().cloned().unwrap_or_default();
    let name = credits
        .iter()
        .map(|c| format!("{}{}", name(c).unwrap_or(""), c["joinphrase"].as_str().unwrap_or("")))
        .collect::<String>();
    let id = credits.first().and_then(|c| c["artist"]["id"].as_str()).unwrap_or("").to_string();
    (name, id)
}

/// Which release a recording gets filed under. Lower wins, compared left to right:
/// official > no secondary types (compilation, live, DJ-mix, soundtrack, ...) >
/// released by the track's own artist > not a regional/deluxe/remastered edition >
/// album > EP > single > earliest date.
fn release_rank(r: &Value, track_artist_mbid: &str) -> (u8, u8, u8, u8, u8, (u16, u8, u8)) {
    const EDITION_WORDS: &[&str] = &["edition", "deluxe", "japan", "tour", "anniversary", "expanded", "remaster"];
    let label =
        format!("{} {}", r["title"].as_str().unwrap_or(""), r["disambiguation"].as_str().unwrap_or("")).to_lowercase();
    let edition = if EDITION_WORDS.iter().any(|w| label.contains(w)) { 1 } else { 0 };
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
    (official, plain, own, edition, ptype, date_key(r["date"].as_str().unwrap_or("")))
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
            "artist-credit": [{"name": "credited-as", "joinphrase": "", "artist": {"id": artist, "name": artist}}],
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
            release("Pablo Honey: Japan Edition", "Official", "1992-12-01", "Album", &[], "radiohead"),
        ]);
        let r = parse_recording(&v, "rec").unwrap();
        assert_eq!(r.album_mbid, "pablo-honey", "earlier Japan edition loses to the plain album");
        assert_eq!(r.album_artist, "radiohead", "the entity's current name, not the credited spelling");
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
             "releases": [{"status": "Official", "release-group": {"primary-type": "Album"}}]},
            {"id": "live", "score": 100, "length": 410000, "artist-credit": q, "releases": [{}, {}]},
            {"id": "karaoke", "artist-credit": q, "score": 100, "length": 238500, "disambiguation": "karaoke version", "releases": [{}]},
            {"id": "studio", "artist-credit": q, "score": 95, "length": 238000, "releases": [{"status": "Official", "release-group": {"primary-type": "Album"}}, {}]},
            {"id": "remaster", "artist-credit": q, "score": 95, "length": 238000, "releases": [{"release-group": {"primary-type": "Album", "secondary-types": ["Compilation"]}}, {}, {}, {}, {}]},
            {"id": "comp-copy", "artist-credit": q, "score": 95, "length": 240000, "releases": [{}]},
            {"id": "junk", "artist-credit": q, "score": 40, "length": 238000, "releases": []}
        ]});
        assert_eq!(
            pick_search_hit(&v, "Queen", "Don't Stop Me Now", Some(240000)).as_deref(),
            Some("studio"),
            "album take beats an exact-length compilation copy"
        );
        assert_eq!(
            pick_search_hit(&v, "queen", "Don't Stop Me Now", None).as_deref(),
            Some("studio"),
            "no length: studio album beats a bigger pile of compilations"
        );
        assert_eq!(pick_search_hit(&json!({"recordings": [{"id": "junk", "score": 40}]}), "", "x", None), None);
        let rmx = json!({"recordings": [
            {"id": "rmx", "title": "Counting Stars (Voicedude rmx)", "artist-credit": q, "score": 100, "length": 257000,
             "releases": [{"status": "Official", "release-group": {"primary-type": "Album"}}]},
            {"id": "orig", "title": "Counting Stars", "artist-credit": q, "score": 100, "length": 257000, "releases": [{}]}
        ]});
        assert_eq!(pick_search_hit(&rmx, "Queen", "Counting Stars", Some(257000)).as_deref(), Some("orig"));
        assert_eq!(pick_search_hit(&rmx, "Queen", "Counting Stars (Voicedude rmx)", None).as_deref(), Some("rmx"));
        // A DJ-pool intro edit on a pseudo-release loses to the plain song on a single, even when
        // the edit's title carries words the filter doesn't know.
        let pool = json!({"recordings": [
            {"id": "pool", "title": "Old Town Road (Fiinest Clap intro) (dirty)", "artist-credit": q, "score": 100, "length": 113000,
             "releases": [{"title": "Unclustered Files", "status": "Pseudo-Release", "release-group": {"primary-type": "Album"}}]},
            {"id": "single", "title": "Old Town Road", "artist-credit": q, "score": 100, "length": 113000,
             "releases": [{"title": "Old Town Road", "status": "Official", "release-group": {"primary-type": "Single"}}]},
            {"id": "weird", "title": "Old Town Road (Zyzz edition)", "artist-credit": q, "score": 100, "length": 113000,
             "releases": [{"title": "X", "status": "Official", "release-group": {"primary-type": "Single"}}]}
        ]});
        assert_eq!(pick_search_hit(&pool, "Queen", "Old Town Road", Some(113000)).as_deref(), Some("single"));
        assert_eq!(pick_search_hit(&pool, "Queen", "Old Town Road", Some(250000)), None, "2 min off: no match");
        assert_eq!(strip_feat("APT. (feat. Bruno Mars)"), "APT.");
        assert_eq!(strip_feat("Blame feat. John Newman"), "Blame");
        assert_eq!(strip_feat("Live and Let Die"), "Live and Let Die");
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
