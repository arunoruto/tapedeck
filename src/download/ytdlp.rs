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

    /// Where to download from, in order of preference: YouTube Music's song result (album
    /// audio, exact lengths, no music-video intros), then the plain YouTube search's closest
    /// hit. Both are kept, because YouTube refuses the odd download with a 403.
    async fn candidates(&self, track: &Track) -> Result<Vec<String>> {
        let mut urls = vec![];
        match self.pick_music(track).await {
            Ok(Some(url)) => urls.push(url),
            Ok(None) => {}
            Err(e) => tracing::debug!("youtube music search failed: {e:#}"),
        }
        let query = format!("ytsearch{CANDIDATES}:{} - {}", track.artist, track.title);
        let out = self.run(&["--flat-playlist", "-j", "--no-warnings", "--quiet"], query.as_ref()).await?;
        let hits: Vec<Value> = out.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        match pick_closest(&hits, track.duration_ms) {
            Ok(best) => urls.extend(
                best["webpage_url"]
                    .as_str()
                    .or(best["url"].as_str())
                    .map(str::to_owned)
                    .or_else(|| best["id"].as_str().map(|id| format!("https://www.youtube.com/watch?v={id}"))),
            ),
            Err(e) if urls.is_empty() => return Err(e),
            Err(e) => tracing::debug!("plain search has no fitting hit: {e:#}"),
        }
        ensure!(!urls.is_empty(), "no search results for {query:?}");
        urls.dedup();
        Ok(urls)
    }

    /// Download one URL and check the file is about as long as the recording.
    async fn fetch(&self, track: &Track, url: &str, dest_dir: &Path) -> Result<PathBuf> {
        let mut cmd = tokio::process::Command::new(&self.bin);
        if let Some(ffmpeg) = &self.ffmpeg {
            cmd.args(["--ffmpeg-location", ffmpeg]);
        }
        let out = cmd
            .args(["-x", "--audio-format", &self.format, "--no-playlist", "--no-warnings", "--quiet"])
            .args(["--retries", "3", "--fragment-retries", "3"])
            .args(["--no-simulate", "--print", "after_move:filepath"])
            .arg("-o")
            .arg(dest_dir.join("%(id)s.%(ext)s"))
            .arg(url)
            .output()
            .await
            .with_context(|| format!("running {}", self.bin))?;
        ensure!(out.status.success(), "yt-dlp failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        let path = String::from_utf8_lossy(&out.stdout)
            .lines()
            .last()
            .map(str::to_owned)
            .context("yt-dlp printed no output path")?;
        let path = PathBuf::from(path);
        // Last line of defence: the file itself must be about as long as the recording.
        if let (Some(want), Some(got)) = (track.duration_ms, file_secs(&path)) {
            let want = want as f64 / 1000.0;
            if (got - want).abs() > MAX_DRIFT_SECS {
                let _ = std::fs::remove_file(&path);
                bail!("downloaded {got:.0}s of audio for a {want:.0}s recording");
            }
        }
        Ok(path)
    }

    /// YouTube Music's "Songs" tab. The flat search carries no durations, so the top hits are
    /// looked up once more (one yt-dlp call) and the song whose length fits the recording wins.
    async fn pick_music(&self, track: &Track) -> Result<Option<String>> {
        let q = urlencoding::encode(&format!("{} {}", track.artist, track.title)).into_owned();
        let search = format!("https://music.youtube.com/search?q={q}#songs");
        let end = CANDIDATES.to_string();
        let out = self
            .run(&["--flat-playlist", "-j", "--no-warnings", "--quiet", "--playlist-end", &end], search.as_ref())
            .await?;
        let ids: Vec<String> = out
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok()?["id"].as_str().map(str::to_owned))
            .collect();
        if ids.is_empty() {
            return Ok(None);
        }
        let mut cmd = tokio::process::Command::new(&self.bin);
        cmd.args(["-j", "--no-warnings", "--quiet", "--no-playlist"]);
        for id in &ids {
            cmd.arg(format!("https://music.youtube.com/watch?v={id}"));
        }
        let out = cmd.output().await.with_context(|| format!("running {}", self.bin))?;
        let hits: Vec<Value> =
            String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        Ok(pick_song(&hits, track.duration_ms, &track.title)
            .and_then(|h| h["id"].as_str())
            .map(|id| format!("https://music.youtube.com/watch?v={id}")))
    }
}

/// Among YouTube Music songs: within the drift limit, the one titled like the recording, then
/// the closest length. `None` when nothing fits, so the caller can try the broader search.
fn pick_song<'a>(hits: &'a [Value], want_ms: Option<i64>, title: &str) -> Option<&'a Value> {
    let want = want_ms.map(|ms| ms as f64 / 1000.0);
    let norm = |s: &str| {
        s.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).collect::<Vec<_>>().join(" ")
    };
    let wanted_title = norm(&crate::musicbrainz::strip_feat(title));
    hits.iter()
        .filter_map(|h| {
            let drift = match (want, h["duration"].as_f64()) {
                (Some(w), Some(d)) => (d - w).abs(),
                (None, _) => 0.0,
                (Some(_), None) => return None,
            };
            (drift <= MAX_DRIFT_SECS).then_some((h, drift))
        })
        .min_by(|(a, da), (b, db)| {
            let exact = |h: &Value| norm(h["track"].as_str().or(h["title"].as_str()).unwrap_or("")) == wanted_title;
            exact(b).cmp(&exact(a)).then(da.total_cmp(db))
        })
        .map(|(h, _)| h)
}

/// Length of a downloaded file, to catch a video cut that slipped through a search without
/// durations. `None` when the file can't be read; the tagging step will complain then.
fn file_secs(path: &Path) -> Option<f64> {
    use lofty::{file::AudioFile, probe::Probe};
    let f = Probe::open(path).ok()?.options(lofty::config::ParseOptions::new().read_tags(false)).read().ok()?;
    Some(f.properties().duration().as_secs_f64())
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
        let urls = match &track.source_url {
            Some(u) => vec![u.clone()],
            None => self.candidates(track).await?,
        };
        let mut errors = vec![];
        for url in &urls {
            match self.fetch(track, url, dest_dir).await {
                Ok(path) => return Ok(Downloaded { path, source_url: Some(url.clone()) }),
                Err(e) => errors.push(format!("{url}: {e:#}")),
            }
        }
        bail!("{}; set a URL with `td source`", errors.join("; "))
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

    #[test]
    fn music_songs_prefer_fitting_length_then_exact_title() {
        let hits = vec![
            json!({"id": "live", "track": "Apricots (Live at Glastonbury)", "duration": 250.0}),
            json!({"id": "song", "track": "Apricots", "duration": 247.0}),
            json!({"id": "nodur", "track": "Apricots"}),
            json!({"id": "long", "track": "Apricots", "duration": 400.0}),
        ];
        assert_eq!(pick_song(&hits, Some(249_000), "Apricots").unwrap()["id"], "song");
        assert_eq!(pick_song(&hits, Some(249_000), "Apricots (Live at Glastonbury)").unwrap()["id"], "live");
        assert!(pick_song(&hits, Some(120_000), "Apricots").is_none(), "nothing within 20 s: let the fallback try");
        assert_eq!(pick_song(&hits, None, "Apricots").unwrap()["id"], "song", "no length: the exact title wins");
    }
}
