//! Where audio comes from. One implementation today (yt-dlp); Soulseek etc. plug in here.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use async_trait::async_trait;

use crate::{config::Config, db::Track};

pub mod ytdlp;

pub struct Downloaded {
    pub path: PathBuf,
    /// Where the audio came from, so it can be inspected or reused.
    pub source_url: Option<String>,
}

#[async_trait]
pub trait Downloader: Send + Sync {
    fn name(&self) -> &'static str;
    /// Fetch audio for `track` into `dest_dir`. Honour `track.source_url` when set.
    /// Tagging and filing are not the downloader's job.
    async fn download(&self, track: &Track, dest_dir: &Path) -> Result<Downloaded>;
    /// Metadata of a link (title, channel, duration, …) without downloading it.
    async fn probe(&self, url: &str) -> Result<serde_json::Value>;
    /// A playlist's title, owner and entries (title, channel, duration, id, url), without
    /// looking at each video.
    async fn playlist(&self, url: &str) -> Result<serde_json::Value>;
}

pub fn from_config(cfg: &Config) -> Result<Box<dyn Downloader>> {
    match cfg.download.backend.as_str() {
        "ytdlp" => Ok(Box::new(ytdlp::YtDlp::new(&cfg.download))),
        other => bail!("unknown download.backend {other:?}"),
    }
}
