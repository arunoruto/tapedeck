//! Plex Media Server playlists via its HTTP API.
//! Flow: refresh the section → map our files to Plex ratingKeys → replace the playlist.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::Value;

use super::{RemoteItem, RemotePlaylist, SyncTarget};
use crate::config;

pub struct Plex {
    cfg: config::Plex,
    token: String,
    http: Client,
}

impl Plex {
    pub fn new(cfg: config::Plex) -> Self {
        // `Config::load` has already turned token_file into token.
        let token = cfg.token.clone().unwrap_or_default();
        Self { cfg, token, http: Client::new() }
    }

    async fn call(&self, method: reqwest::Method, path: &str) -> Result<Value> {
        let res = self
            .http
            .request(method, format!("{}{}", self.cfg.url.trim_end_matches('/'), path))
            .header("X-Plex-Token", &self.token)
            .header("Accept", "application/json")
            .send()
            .await?
            .error_for_status()
            .with_context(|| format!("plex {path}"))?;
        if res.content_length() == Some(0) {
            return Ok(Value::Null);
        }
        Ok(res.json().await.unwrap_or(Value::Null))
    }

    async fn get(&self, path: &str) -> Result<Value> {
        self.call(reqwest::Method::GET, path).await
    }

    /// Section key plus the folders it scans, so we can tell up front whether our files are even visible.
    async fn section(&self) -> Result<(String, Vec<String>)> {
        let v = self.get("/library/sections").await?;
        let dir = v["MediaContainer"]["Directory"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|d| d["title"].as_str() == Some(&self.cfg.section))
            .with_context(|| format!("plex has no library section named {:?}", self.cfg.section))?;
        let key = dir["key"].as_str().context("plex section has no key")?.to_string();
        let locations = dir["Location"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| l["path"].as_str().map(|p| p.trim_end_matches('/').to_string()))
            .collect();
        Ok((key, locations))
    }

    /// Kick a scan and wait for it to finish so freshly filed tracks are matchable.
    async fn refresh(&self, key: &str) -> Result<()> {
        self.get(&format!("/library/sections/{key}/refresh")).await?;
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let v = self.get("/library/sections").await?;
            let busy = v["MediaContainer"]["Directory"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|d| d["key"].as_str() == Some(key) && d["refreshing"].as_bool() == Some(true));
            if !busy {
                return Ok(());
            }
        }
        bail!("plex section {key} still refreshing after 120s")
    }

    /// Plex file path → ratingKey for every track in the section.
    // ponytail: lists the whole section each sync; cache ratingKeys in SQLite when the library gets big enough to notice.
    async fn file_index(&self, key: &str) -> Result<HashMap<String, String>> {
        let v = self.get(&format!("/library/sections/{key}/all?type=10")).await?;
        let mut idx = HashMap::new();
        for m in v["MediaContainer"]["Metadata"].as_array().into_iter().flatten() {
            let Some(rk) = m["ratingKey"].as_str() else { continue };
            for media in m["Media"].as_array().into_iter().flatten() {
                for part in media["Part"].as_array().into_iter().flatten() {
                    if let Some(f) = part["file"].as_str() {
                        idx.insert(f.to_string(), rk.to_string());
                    }
                }
            }
        }
        Ok(idx)
    }

    /// Inverse of `remote_path`: what Plex calls the file → where it is for us.
    fn local_path(&self, remote: &str) -> PathBuf {
        let hit = self
            .cfg
            .path_map
            .iter()
            .filter(|(_, to)| remote.starts_with(to.trim_end_matches('/')))
            .max_by_key(|(_, to)| to.len());
        match hit {
            Some((from, to)) => {
                PathBuf::from(format!("{}{}", from.trim_end_matches('/'), &remote[to.trim_end_matches('/').len()..]))
            }
            None => PathBuf::from(remote),
        }
    }

    fn remote_path(&self, local: &Path) -> String {
        let s = local.to_string_lossy();
        // Longest matching prefix wins.
        let hit = self
            .cfg
            .path_map
            .iter()
            .filter(|(from, _)| s.starts_with(from.as_str()))
            .max_by_key(|(from, _)| from.len());
        match hit {
            Some((from, to)) => format!("{}{}", to.trim_end_matches('/'), &s[from.trim_end_matches('/').len()..]),
            None => s.into_owned(),
        }
    }

    async fn delete_existing(&self, title: &str) -> Result<()> {
        for p in self.list_playlists().await? {
            if p.title == title {
                self.delete_playlist(&p.id).await?;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl SyncTarget for Plex {
    fn name(&self) -> &'static str {
        "plex"
    }

    async fn list_playlists(&self) -> Result<Vec<RemotePlaylist>> {
        let v = self.get("/playlists?playlistType=audio").await?;
        Ok(v["MediaContainer"]["Metadata"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| {
                Some(RemotePlaylist {
                    id: p["ratingKey"].as_str()?.to_string(),
                    title: p["title"].as_str().unwrap_or("").to_string(),
                    tracks: p["leafCount"].as_i64().unwrap_or(0),
                    smart: p["smart"].as_bool().unwrap_or(false) || p["smart"].as_i64() == Some(1),
                })
            })
            .collect())
    }

    async fn playlist_items(&self, id: &str) -> Result<Vec<RemoteItem>> {
        let v = self.get(&format!("/playlists/{id}/items")).await?;
        Ok(v["MediaContainer"]["Metadata"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| {
                Some(RemoteItem {
                    id: m["ratingKey"].as_str()?.to_string(),
                    artist: m["originalTitle"].as_str().or(m["grandparentTitle"].as_str()).unwrap_or("").to_string(),
                    title: m["title"].as_str().unwrap_or("").to_string(),
                    file: m["Media"][0]["Part"][0]["file"].as_str().map(|f| self.local_path(f)),
                })
            })
            .collect())
    }

    async fn delete_playlist(&self, id: &str) -> Result<()> {
        self.call(reqwest::Method::DELETE, &format!("/playlists/{id}")).await.map(drop)
    }

    async fn sync_playlist(&self, title: &str, files: &[PathBuf]) -> Result<String> {
        let (key, locations) = self.section().await?;
        if let Some(first) = files.first() {
            let remote = self.remote_path(first);
            if !locations.iter().any(|l| remote.starts_with(l.as_str())) {
                bail!(
                    "{remote} is outside the folders plex section {:?} scans ({}); add the library to the section or fix sync.plex.path_map",
                    self.cfg.section,
                    locations.join(", ")
                );
            }
        }
        self.refresh(&key).await?;
        let index = self.file_index(&key).await?;

        let mut keys = vec![];
        for f in files {
            match index.get(&self.remote_path(f)) {
                Some(rk) => keys.push(rk.as_str()),
                None => tracing::warn!("plex does not know {} (check sync.plex.path_map)", f.display()),
            }
        }
        if keys.is_empty() {
            bail!("none of the {} files are known to plex", files.len());
        }

        let machine = self.get("/").await?["MediaContainer"]["machineIdentifier"]
            .as_str()
            .context("plex did not return machineIdentifier")?
            .to_string();
        // ponytail: delete + recreate instead of diffing items; Plex playlist history is not something we promise.
        self.delete_existing(title).await?;
        let uri = format!("server://{machine}/com.plexapp.plugins.library/library/metadata/{}", keys.join(","));
        let v = self
            .call(
                reqwest::Method::POST,
                &format!(
                    "/playlists?type=audio&smart=0&title={}&uri={}",
                    urlencoding::encode(title),
                    urlencoding::encode(&uri)
                ),
            )
            .await?;
        let rk =
            v["MediaContainer"]["Metadata"][0]["ratingKey"].as_str().context("plex create returned no ratingKey")?;
        tracing::info!("plex: {title} → {} tracks (ratingKey {rk})", keys.len());
        Ok(rk.to_string())
    }
}
