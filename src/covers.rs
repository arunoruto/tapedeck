//! Cover art for tracks taken from YouTube as the video itself (`youtube:<video id>`): they have
//! no MusicBrainz release, so the Cover Art Archive has nothing. The video's thumbnail stands in,
//! cropped square from the middle (where a Topic video's album art sits), embedded in the file
//! and saved as `cover.jpg` in the album folder when it has none. Plex reads either after an
//! album refresh; the Subsonic server serves the embedded one.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use lofty::{
    config::WriteOptions,
    picture::{MimeType, Picture, PictureType},
    prelude::*,
    probe::Probe,
    tag::Tag,
};
use tokio::sync::mpsc::Sender;

use crate::{
    App, Event,
    app::{info, progress_at},
    db::Track,
};

/// The video id of a track taken as the video itself.
pub fn youtube_id(t: &Track) -> Option<&str> {
    t.recording_mbid.strip_prefix("youtube:").filter(|id| !id.is_empty())
}

/// Whether the file carries a picture already.
fn has_picture(path: &Path) -> bool {
    Probe::open(path).and_then(|p| p.read()).is_ok_and(|f| f.tags().iter().any(|t| !t.pictures().is_empty()))
}

/// Put `jpg` into the file as its front cover, replacing an earlier one.
fn embed(path: &Path, jpg: &[u8]) -> Result<()> {
    let mut file = Probe::open(path)?.read().with_context(|| format!("reading tags of {}", path.display()))?;
    if file.primary_tag().is_none() {
        let ty = file.primary_tag_type();
        file.insert_tag(Tag::new(ty));
    }
    let tag = file.primary_tag_mut().expect("tag inserted above");
    tag.remove_picture_type(PictureType::CoverFront);
    tag.push_picture(
        Picture::unchecked(jpg.to_vec()).pic_type(PictureType::CoverFront).mime_type(MimeType::Jpeg).build(),
    );
    tag.save_to_path(path, WriteOptions::default())
        .with_context(|| format!("writing the cover into {}", path.display()))
}

impl App {
    /// The ffmpeg binary: `[download] ffmpeg_bin` (a binary or the folder holding it) or PATH's.
    pub(crate) fn ffmpeg(&self) -> PathBuf {
        match self.config.download.ffmpeg_bin.as_deref().map(Path::new) {
            Some(p) if p.is_dir() => p.join("ffmpeg"),
            Some(p) => p.to_path_buf(),
            None => PathBuf::from("ffmpeg"),
        }
    }

    /// The video's thumbnail, largest first, cropped to the square in its middle.
    async fn thumbnail(&self, video: &str) -> Result<Vec<u8>> {
        let mut raw = None;
        for size in ["maxresdefault", "hqdefault"] {
            let res = crate::http().get(format!("https://i.ytimg.com/vi/{video}/{size}.jpg")).send().await?;
            if res.status().is_success() {
                raw = Some(res.bytes().await?.to_vec());
                break;
            }
        }
        let raw = raw.with_context(|| format!("no thumbnail for video {video}"))?;
        let dir = self.config.tmp_dir();
        std::fs::create_dir_all(&dir)?;
        let (src, dst) = (dir.join(format!("{video}.thumb.jpg")), dir.join(format!("{video}.cover.jpg")));
        std::fs::write(&src, &raw)?;
        let out = tokio::process::Command::new(self.ffmpeg())
            .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
            .arg(&src)
            .args(["-vf", "crop=min(iw\\,ih):min(iw\\,ih)", "-q:v", "2"])
            .arg(&dst)
            .output()
            .await
            .context("running ffmpeg");
        let _ = std::fs::remove_file(&src);
        let out = out?;
        ensure!(out.status.success(), "ffmpeg: {}", String::from_utf8_lossy(&out.stderr).trim());
        let jpg = std::fs::read(&dst)?;
        let _ = std::fs::remove_file(&dst);
        Ok(jpg)
    }

    /// Give a filed `youtube:` track its video's thumbnail as cover, unless it has a picture.
    /// The album folder gets it as `cover.jpg` when it has no cover file yet. Returns the audio
    /// path when something was written.
    pub(crate) async fn youtube_cover(&self, t: &Track) -> Result<Option<PathBuf>> {
        let (Some(video), Some(rel)) = (youtube_id(t), &t.path) else { return Ok(None) };
        let path = self.config.library.join(rel);
        if has_picture(&path) {
            return Ok(None);
        }
        let jpg = self.thumbnail(video).await?;
        embed(&path, &jpg)?;
        if let Some(dir) = path.parent()
            && !crate::steam::COVER_NAMES.iter().any(|n| dir.join(n).is_file())
        {
            std::fs::write(dir.join("cover.jpg"), &jpg)
                .with_context(|| format!("writing {}/cover.jpg", dir.display()))?;
        }
        Ok(Some(path))
    }

    /// `td covers`: every track taken from YouTube without a cover gets its thumbnail; Plex is
    /// told to refresh those albums.
    pub(crate) async fn covers(&self, dry_run: bool, tx: &Sender<Event>) -> Result<()> {
        let tracks: Vec<Track> = self
            .db()
            .all_tracks()?
            .into_iter()
            .filter(|t| t.status == "ready" && youtube_id(t).is_some())
            .filter(|t| t.path.as_deref().is_some_and(|rel| !has_picture(&self.config.library.join(rel))))
            .collect();
        if dry_run {
            for t in &tracks {
                info(tx, format!("would add a cover to {} - {}", t.artist, t.title)).await?;
            }
            return info(tx, format!("{} tracks from YouTube have no cover", tracks.len())).await;
        }
        let mut files = vec![];
        for (i, t) in tracks.iter().enumerate() {
            progress_at(tx, i + 1, tracks.len(), format!("{} - {}", t.artist, t.title)).await?;
            match self.youtube_cover(t).await {
                Ok(Some(f)) => files.push(f),
                Ok(None) => {}
                Err(e) => info(tx, format!("{} - {}: {e:#}", t.artist, t.title)).await?,
            }
        }
        self.refresh_albums(&files, tx).await?;
        info(tx, format!("{} covers added", files.len())).await
    }

    /// Ask the sync targets to re-read the albums of `files` (new cover, new sidecars).
    pub(crate) async fn refresh_albums(&self, files: &[PathBuf], tx: &Sender<Event>) -> Result<()> {
        if files.is_empty() {
            return Ok(());
        }
        for target in &self.targets {
            if let Err(e) = target.refresh_lyrics(files).await {
                info(tx, format!("{}: album refresh: {e:#}", target.name())).await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_youtube_tracks_have_a_video() {
        let t = |mbid: &str| Track { recording_mbid: mbid.into(), ..Default::default() };
        assert_eq!(youtube_id(&t("youtube:5crpQyTRytc")), Some("5crpQyTRytc"));
        assert_eq!(youtube_id(&t("youtube:")), None);
        assert_eq!(youtube_id(&t("8c5e4b5a-1f2b-4c3d-9e8f-0a1b2c3d4e5f")), None);
    }
}
