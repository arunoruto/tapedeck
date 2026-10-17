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

use super::SyncTarget;
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

    async fn section_key(&self) -> Result<String> {
        let v = self.get("/library/sections").await?;
        v["MediaContainer"]["Directory"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|d| d["title"].as_str() == Some(&self.cfg.section))
            .and_then(|d| d["key"].as_str())
            .map(str::to_owned)
            .with_context(|| format!("plex has no library section named {:?}", self.cfg.section))
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
        let v = self.get("/playlists?playlistType=audio").await?;
        for p in v["MediaContainer"]["Metadata"].as_array().into_iter().flatten() {
            if p["title"].as_str() == Some(title)
                && let Some(rk) = p["ratingKey"].as_str()
            {
                self.call(reqwest::Method::DELETE, &format!("/playlists/{rk}")).await?;
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

    async fn sync_playlist(&self, title: &str, files: &[PathBuf]) -> Result<String> {
        let key = self.section_key().await?;
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
