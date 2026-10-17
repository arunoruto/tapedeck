//! Where playlists go. Targets only ever see a title and absolute file paths.

use std::path::PathBuf;

use anyhow::{Result, bail};
use async_trait::async_trait;

use crate::config::Config;

pub mod plex;

#[async_trait]
pub trait SyncTarget: Send + Sync {
    fn name(&self) -> &'static str;
    /// Create or replace the playlist `title` with these local files. Returns the remote id.
    async fn sync_playlist(&self, title: &str, files: &[PathBuf]) -> Result<String>;
}

pub fn from_config(cfg: &Config) -> Result<Vec<Box<dyn SyncTarget>>> {
    cfg.sync
        .targets
        .iter()
        .map(|t| -> Result<Box<dyn SyncTarget>> {
            match t.as_str() {
                "plex" => Ok(Box::new(plex::Plex::new(cfg.sync.plex.clone().expect("validated in Config::load")))),
                other => bail!("unknown sync target {other:?}"),
            }
        })
        .collect()
}
