//! ListenBrainz "created for you" playlists (Weekly Jams, Weekly Exploration, Daily Jams, ...).
//! Payloads are JSPF; we pull the few fields we need out of `serde_json::Value`.

use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;

const API: &str = "https://api.listenbrainz.org/1";
const JSPF_EXT: &str = "https://musicbrainz.org/doc/jspf#playlist";

pub struct ListenBrainz {
    http: Client,
    token: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LbPlaylist {
    pub mbid: String,
    pub title: String,
    /// `source_patch`, e.g. "weekly-jams". Stable across weeks, unlike the title.
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LbTrack {
    pub recording_mbid: String,
    pub title: String,
    pub artist: String,
}

impl ListenBrainz {
    pub fn new(http: Client, token: Option<String>) -> Self {
        Self { http, token }
    }

    async fn get(&self, path: &str) -> Result<Value> {
        let mut req = self.http.get(format!("{API}/{path}"));
        if let Some(t) = &self.token {
            req = req.header("Authorization", format!("Token {t}"));
        }
        let res = req.send().await?.error_for_status().with_context(|| format!("listenbrainz {path}"))?;
        Ok(res.json().await?)
    }

    pub async fn created_for(&self, user: &str) -> Result<Vec<LbPlaylist>> {
        Ok(parse_created_for(&self.get(&format!("user/{user}/playlists/createdfor?count=50")).await?))
    }

    pub async fn tracks(&self, playlist_mbid: &str) -> Result<Vec<LbTrack>> {
        Ok(parse_tracks(&self.get(&format!("playlist/{playlist_mbid}")).await?))
    }
}

pub fn parse_created_for(v: &Value) -> Vec<LbPlaylist> {
    let mut out = vec![];
    for p in v["playlists"].as_array().into_iter().flatten() {
        let p = &p["playlist"];
        let Some(id) = p["identifier"].as_str() else { continue };
        let kind = p["extension"][JSPF_EXT]["additional_metadata"]["algorithm_metadata"]["source_patch"]
            .as_str()
            .unwrap_or("unknown");
        out.push(LbPlaylist {
            mbid: last_segment(id),
            title: p["title"].as_str().unwrap_or(id).to_string(),
            kind: kind.to_string(),
        });
    }
    out
}

pub fn parse_tracks(v: &Value) -> Vec<LbTrack> {
    let mut out = vec![];
    for t in v["playlist"]["track"].as_array().into_iter().flatten() {
        // JSPF `identifier` is a string in older payloads and an array in newer ones.
        let ident = match &t["identifier"] {
            Value::String(s) => Some(s.as_str()),
            Value::Array(a) => a.first().and_then(Value::as_str),
            _ => None,
        };
        let Some(ident) = ident else { continue };
        out.push(LbTrack {
            recording_mbid: last_segment(ident),
            title: t["title"].as_str().unwrap_or("").to_string(),
            artist: t["creator"].as_str().unwrap_or("").to_string(),
        });
    }
    out
}

fn last_segment(url: &str) -> String {
    url.trim_end_matches('/').rsplit('/').next().unwrap_or(url).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_created_for() {
        let v = json!({"playlists": [{"playlist": {
            "identifier": "https://listenbrainz.org/playlist/abc-123",
            "title": "Weekly Jams for rob, week of 2026-08-24 Mon",
            "extension": {JSPF_EXT: {"additional_metadata": {"algorithm_metadata": {"source_patch": "weekly-jams"}}}}
        }}]});
        assert_eq!(
            parse_created_for(&v),
            vec![LbPlaylist {
                mbid: "abc-123".into(),
                title: "Weekly Jams for rob, week of 2026-08-24 Mon".into(),
                kind: "weekly-jams".into()
            }]
        );
    }

    #[test]
    fn parses_tracks_with_string_or_array_identifier() {
        let v = json!({"playlist": {"track": [
            {"identifier": "https://musicbrainz.org/recording/r1", "title": "A", "creator": "X"},
            {"identifier": ["https://musicbrainz.org/recording/r2"], "title": "B", "creator": "Y"},
            {"title": "no id, skipped"}
        ]}});
        let t = parse_tracks(&v);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].recording_mbid, "r1");
        assert_eq!(t[1], LbTrack { recording_mbid: "r2".into(), title: "B".into(), artist: "Y".into() });
    }
}
