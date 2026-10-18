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
    /// ISO 8601 creation time as ListenBrainz reports it.
    pub date: String,
}

/// ListenBrainz keeps history (and occasionally generates a week twice); we only want the
/// current playlist of each kind.
pub fn newest_per_kind(playlists: Vec<LbPlaylist>) -> Vec<LbPlaylist> {
    let mut newest: Vec<LbPlaylist> = vec![];
    for p in playlists {
        match newest.iter_mut().find(|n| n.kind == p.kind) {
            Some(n) if n.date >= p.date => {}
            Some(n) => *n = p,
            None => newest.push(p),
        }
    }
    newest
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

    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    /// Submit one listen: `listened_at = None` means "playing now", otherwise a finished listen
    /// at that Unix time. The recording id goes along so ListenBrainz maps it exactly.
    pub async fn submit_listen(&self, t: &crate::db::Track, listened_at: Option<i64>) -> Result<()> {
        let token = self.token.as_ref().context("no listenbrainz token")?;
        let mut meta = serde_json::json!({
            "artist_name": t.artist,
            "track_name": t.title,
            "release_name": t.album,
            "additional_info": {
                "media_player": "tapedeck",
                "submission_client": "tapedeck",
                "submission_client_version": env!("CARGO_PKG_VERSION"),
                "duration_ms": t.duration_ms,
            }
        });
        if crate::library::is_uuid(&t.recording_mbid) {
            meta["additional_info"]["recording_mbid"] = serde_json::json!(t.recording_mbid);
        }
        let mut item = serde_json::json!({ "track_metadata": meta });
        if let Some(at) = listened_at {
            item["listened_at"] = serde_json::json!(at);
        }
        let body = serde_json::json!({
            "listen_type": if listened_at.is_some() { "single" } else { "playing_now" },
            "payload": [item],
        });
        let res = self
            .http
            .post(format!("{API}/submit-listens"))
            .header("Authorization", format!("Token {token}"))
            .json(&body)
            .send()
            .await
            .context("submit-listens")?;
        anyhow::ensure!(
            res.status().is_success(),
            "submit-listens: {} {}",
            res.status(),
            res.text().await.unwrap_or_default()
        );
        Ok(())
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
            date: p["date"].as_str().unwrap_or("").to_string(),
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
            "date": "2026-08-24T00:05:00+00:00",
            "extension": {JSPF_EXT: {"additional_metadata": {"algorithm_metadata": {"source_patch": "weekly-jams"}}}}
        }}]});
        assert_eq!(
            parse_created_for(&v),
            vec![LbPlaylist {
                mbid: "abc-123".into(),
                title: "Weekly Jams for rob, week of 2026-08-24 Mon".into(),
                kind: "weekly-jams".into(),
                date: "2026-08-24T00:05:00+00:00".into(),
            }]
        );
    }

    #[test]
    fn newest_per_kind_drops_history_and_duplicates() {
        let pl = |mbid: &str, kind: &str, date: &str| LbPlaylist {
            mbid: mbid.into(),
            title: String::new(),
            kind: kind.into(),
            date: date.into(),
        };
        let got = newest_per_kind(vec![
            pl("a", "weekly-jams", "2026-10-05T00:01:00+00:00"),
            pl("b", "weekly-jams", "2026-10-05T00:03:30+00:00"),
            pl("c", "weekly-jams", "2026-09-28T00:01:00+00:00"),
            pl("d", "daily-jams", "2026-10-07T00:01:00+00:00"),
        ]);
        assert_eq!(got.iter().map(|p| p.mbid.as_str()).collect::<Vec<_>>(), ["b", "d"]);
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
