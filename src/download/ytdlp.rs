use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use serde_json::Value;

use super::{Downloaded, Downloader};
use crate::{config, db::Track};

/// How far a search hit may be from the MusicBrainz length before we'd rather fail than
/// file a music video intro or a radio edit as the track.
const MAX_DRIFT_SECS: f64 = 20.0;
const CANDIDATES: usize = 5;

pub struct YtDlp {
    bin: String,
    format: String,
    ffmpeg: Option<String>,
}

impl YtDlp {
    pub fn new(cfg: &config::Download) -> Self {
        Self { bin: cfg.ytdlp_bin.clone(), format: cfg.format.clone(), ffmpeg: cfg.ffmpeg_bin.clone() }
    }

    async fn run(&self, args: &[&str], target: &std::ffi::OsStr) -> Result<String> {
        let out = tokio::process::Command::new(&self.bin)
            .args(args)
            .arg(target)
            .output()
            .await
            .with_context(|| format!("running {}", self.bin))?;
        ensure!(out.status.success(), "yt-dlp failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Search and pick the hit whose length is closest to what MusicBrainz says the recording is.
    async fn pick(&self, track: &Track) -> Result<String> {
        let query = format!("ytsearch{CANDIDATES}:{} - {}", track.artist, track.title);
        let out = self.run(&["--flat-playlist", "-j", "--no-warnings", "--quiet"], query.as_ref()).await?;
        let hits: Vec<Value> = out.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        ensure!(!hits.is_empty(), "no search results for {query:?}");
        let best = pick_closest(&hits, track.duration_ms)?;
        best["webpage_url"]
            .as_str()
            .or(best["url"].as_str())
            .map(str::to_owned)
            .or_else(|| best["id"].as_str().map(|id| format!("https://www.youtube.com/watch?v={id}")))
            .context("search hit has no url")
    }
}

fn pick_closest(hits: &[Value], want_ms: Option<i64>) -> Result<&Value> {
    let Some(want) = want_ms.map(|ms| ms as f64 / 1000.0) else { return Ok(&hits[0]) };
    let drift = |h: &Value| h["duration"].as_f64().map(|d| (d - want).abs());
    let best =
        hits.iter().min_by(|a, b| drift(a).unwrap_or(f64::MAX).total_cmp(&drift(b).unwrap_or(f64::MAX))).unwrap();
    match drift(best) {
        Some(d) if d > MAX_DRIFT_SECS => bail!(
            "closest search hit {:?} is {d:.0}s off the {want:.0}s recording; set a URL with `td source`",
            best["title"].as_str().unwrap_or("?")
        ),
        _ => Ok(best),
    }
}

#[async_trait]
impl Downloader for YtDlp {
    fn name(&self) -> &'static str {
        "ytdlp"
    }

    async fn download(&self, track: &Track, dest_dir: &Path) -> Result<Downloaded> {
        std::fs::create_dir_all(dest_dir)?;
        let url = match &track.source_url {
            Some(u) => u.clone(),
            None => self.pick(track).await?,
        };
        let mut cmd = tokio::process::Command::new(&self.bin);
        if let Some(ffmpeg) = &self.ffmpeg {
            cmd.args(["--ffmpeg-location", ffmpeg]);
        }
        let out = cmd
            .args(["-x", "--audio-format", &self.format, "--no-playlist", "--no-warnings", "--quiet"])
            .args(["--no-simulate", "--print", "after_move:filepath"])
            .arg("-o")
            .arg(dest_dir.join("%(id)s.%(ext)s"))
            .arg(&url)
            .output()
            .await
            .with_context(|| format!("running {}", self.bin))?;
        ensure!(out.status.success(), "yt-dlp failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        let path = String::from_utf8_lossy(&out.stdout)
            .lines()
            .last()
            .map(str::to_owned)
            .context("yt-dlp printed no output path")?;
        Ok(Downloaded { path: PathBuf::from(path), source_url: Some(url) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn picks_by_duration_and_rejects_far_misses() {
        let hits = vec![
            json!({"id": "video", "title": "Official Video", "duration": 301.0}),
            json!({"id": "audio", "title": "Audio", "duration": 243.0}),
            json!({"id": "nodur", "title": "?", "duration": null}),
        ];
        assert_eq!(pick_closest(&hits, Some(240_000)).unwrap()["id"], "audio");
        assert_eq!(pick_closest(&hits, None).unwrap()["id"], "video", "no length known: trust the search order");
        assert!(pick_closest(&hits, Some(60_000)).unwrap_err().to_string().contains("off the 60s recording"));
    }
}
