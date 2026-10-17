use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;

use super::Downloader;
use crate::{config, db::Track};

pub struct YtDlp {
    bin: String,
    format: String,
}

impl YtDlp {
    pub fn new(cfg: &config::Download) -> Self {
        Self { bin: cfg.ytdlp_bin.clone(), format: cfg.format.clone() }
    }
}

#[async_trait]
impl Downloader for YtDlp {
    fn name(&self) -> &'static str {
        "ytdlp"
    }

    async fn download(&self, track: &Track, dest_dir: &Path) -> Result<PathBuf> {
        std::fs::create_dir_all(dest_dir)?;
        let target =
            track.source_url.clone().unwrap_or_else(|| format!("ytsearch1:{} - {}", track.artist, track.title));
        let out = tokio::process::Command::new(&self.bin)
            .args(["-x", "--audio-format", &self.format, "--no-playlist", "--no-warnings", "--quiet"])
            .args(["--no-simulate", "--print", "after_move:filepath"])
            .arg("-o")
            .arg(dest_dir.join("%(id)s.%(ext)s"))
            .arg(&target)
            .output()
            .await
            .with_context(|| format!("running {}", self.bin))?;
        ensure!(out.status.success(), "yt-dlp failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        let path = String::from_utf8_lossy(&out.stdout)
            .lines()
            .last()
            .map(str::to_owned)
            .context("yt-dlp printed no output path")?;
        Ok(PathBuf::from(path))
    }
}
