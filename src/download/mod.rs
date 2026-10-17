//! Where audio comes from. One implementation today (yt-dlp); Soulseek etc. plug in here.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use async_trait::async_trait;

use crate::{config::Config, db::Track};

pub mod ytdlp;

#[async_trait]
pub trait Downloader: Send + Sync {
    fn name(&self) -> &'static str;
    /// Fetch audio for `track` into `dest_dir` and return the file written.
    /// Tagging and filing are not the downloader's job.
    async fn download(&self, track: &Track, dest_dir: &Path) -> Result<PathBuf>;
}

pub fn from_config(cfg: &Config) -> Result<Box<dyn Downloader>> {
    match cfg.download.backend.as_str() {
        "ytdlp" => Ok(Box::new(ytdlp::YtDlp::new(&cfg.download))),
        other => bail!("unknown download.backend {other:?}"),
    }
}
