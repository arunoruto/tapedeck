//! Filing downloaded audio into the library: path templating, tagging, moving.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use lofty::config::{ParseOptions, WriteOptions};
use lofty::id3::v2::Frame;
use lofty::mpeg::MpegFile;
use lofty::prelude::*;
use lofty::probe::Probe;
use lofty::tag::{ItemKey, ItemValue, Tag};

use crate::db::Track;

const AUDIO_EXT: &[&str] = &["opus", "ogg", "mp3", "m4a", "aac", "flac", "wav", "wma"];

const TOKENS: &[&str] = &["artist", "album_artist", "album", "year", "track", "track:02", "title", "ext"];

/// Render a Lidarr-style template like `{album_artist}/{album} ({year})/{track:02} - {title}.{ext}`.
/// Each token value is sanitized so it can never introduce a path separator.
pub fn render_template(template: &str, t: &Track, ext: &str) -> Result<PathBuf> {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let end = rest[start..].find('}').map(|e| start + e).context("unclosed { in path_template")?;
        let token = &rest[start + 1..end];
        let value = match token {
            "artist" => t.artist.clone(),
            "album_artist" => t.album_artist.clone(),
            "album" => t.album.clone(),
            "year" => t.year.map(|y| y.to_string()).unwrap_or_else(|| "Unknown".into()),
            "track" => t.track_no.unwrap_or(0).to_string(),
            "track:02" => format!("{:02}", t.track_no.unwrap_or(0)),
            "title" => t.title.clone(),
            "ext" => ext.to_string(),
            other => bail!("unknown token {{{other}}} in path_template, known: {}", TOKENS.join(" ")),
        };
        out.push_str(&sanitize(&value));
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Ok(PathBuf::from(out))
}

/// Fail fast on a bad template at startup instead of on the first download.
pub fn validate_template(template: &str) -> Result<()> {
    let dummy = Track {
        id: 0,
        recording_mbid: String::new(),
        title: "t".into(),
        artist: "a".into(),
        album_artist: "a".into(),
        album: "b".into(),
        year: Some(2000),
        track_no: Some(1),
        path: None,
        status: String::new(),
        source_url: None,
        error: None,
        artist_mbid: None,
        album_mbid: String::new(),
        album_artist_mbid: String::new(),
        duration_ms: None,
    };
    render_template(template, &dummy, "opus").map(drop)
}

/// Make a tag value safe as one path component, the way Lidarr does it: `Title: Sub` becomes
/// `Title - Sub`, `R U Mine?` becomes `R U Mine`, and only truly unrepresentable characters turn into `_`.
fn sanitize(s: &str) -> String {
    let cleaned: String = s
        .replace(": ", " - ")
        .replace(':', "-")
        .replace('"', "'")
        .chars()
        .filter(|c| !matches!(c, '?' | '*' | '<' | '>' | '|'))
        .map(|c| match c {
            '/' | '\\' | '\0' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_end_matches('.').trim();
    if trimmed.is_empty() { "_".into() } else { trimmed.to_string() }
}

/// Write tags from our MusicBrainz-sourced metadata, replacing whatever yt-dlp left behind.
pub fn tag(path: &Path, t: &Track) -> Result<()> {
    let mut file = Probe::open(path)?.read().with_context(|| format!("reading tags of {}", path.display()))?;
    if file.primary_tag().is_none() {
        let ty = file.primary_tag_type();
        file.insert_tag(Tag::new(ty));
    }
    let tag = file.primary_tag_mut().expect("tag inserted above");
    tag.clear();
    tag.set_title(t.title.clone());
    tag.set_artist(t.artist.clone());
    tag.set_album(t.album.clone());
    tag.insert_text(ItemKey::AlbumArtist, t.album_artist.clone());
    tag.insert_text(ItemKey::MusicBrainzRecordingId, t.recording_mbid.clone());
    tag.insert_text(ItemKey::MusicBrainzReleaseId, t.album_mbid.clone());
    tag.insert_text(ItemKey::MusicBrainzReleaseArtistId, t.album_artist_mbid.clone());
    if let Some(id) = &t.artist_mbid {
        tag.insert_text(ItemKey::MusicBrainzArtistId, id.clone());
    }
    if let Some(n) = t.track_no {
        tag.set_track(n as u32);
    }
    if let Some(y) = t.year {
        tag.insert_text(ItemKey::RecordingDate, y.to_string());
    }
    tag.save_to_path(path, WriteOptions::default()).with_context(|| format!("writing tags to {}", path.display()))
}

/// Tag a downloaded file and move it to its templated place. Returns the path relative to `library`.
pub fn file_track(library: &Path, template: &str, t: &Track, downloaded: &Path) -> Result<PathBuf> {
    tag(downloaded, t)?;
    let ext = downloaded.extension().and_then(|e| e.to_str()).unwrap_or("bin");
    let rel = render_template(template, t, ext)?;
    let dest = library.join(&rel);
    std::fs::create_dir_all(dest.parent().context("template has no directory part")?)?;
    move_file(downloaded, &dest)?;
    Ok(rel)
}

/// Move a track already in the library to a new relative path and drop directories left empty.
pub fn move_track(library: &Path, old_rel: &Path, new_rel: &Path) -> Result<()> {
    let (from, to) = (library.join(old_rel), library.join(new_rel));
    std::fs::create_dir_all(to.parent().context("template has no directory part")?)?;
    move_file(&from, &to)?;
    let mut dir = from.parent();
    while let Some(d) = dir
        && d != library
        && std::fs::remove_dir(d).is_ok()
    {
        dir = d.parent();
    }
    Ok(())
}

/// rename, falling back to copy + delete when source and library are on different filesystems.
fn move_file(from: &Path, to: &Path) -> Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to).with_context(|| format!("copying to {}", to.display()))?;
    std::fs::remove_file(from)?;
    Ok(())
}

/// An audio file found outside the library, with whatever identity its tags or name give away.
#[derive(Debug, Clone)]
pub struct Found {
    pub path: PathBuf,
    pub artist: String,
    pub title: String,
    pub recording_mbid: Option<String>,
    pub duration_ms: Option<i64>,
}

/// Recursively list audio files under `dir`, reading tags where present.
pub fn scan(dir: &Path) -> Result<Vec<Found>> {
    let mut out = vec![];
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).with_context(|| format!("reading {}", d.display()))? {
            let p = entry?.path();
            if p.is_dir() {
                stack.push(p);
            } else if p
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| AUDIO_EXT.contains(&e.to_lowercase().as_str()))
            {
                out.push(inspect(&p));
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn inspect(path: &Path) -> Found {
    let (mut artist, mut title) = parse_stem(path.file_stem().and_then(|s| s.to_str()).unwrap_or(""));
    let mut recording_mbid = None;
    let mut duration_ms = None;
    if let Ok(file) = Probe::open(path).and_then(|p| p.read()) {
        duration_ms = Some(file.properties().duration().as_millis() as i64);
        // Picard writes MUSICBRAINZ_TRACKID / a UFID frame, Explo writes ID3 TXXX frames called
        // "MusicBrainz Track Id" or "MusicBrainz_TrackId"; all of them hold the recording MBID.
        let find_mbid = |tag: &lofty::tag::Tag| {
            tag.items().find_map(|item| {
                if !is_recording_id_key(&format!("{:?}", item.key())) {
                    return None;
                }
                let v = match item.value() {
                    ItemValue::Text(s) | ItemValue::Locator(s) => s.clone(),
                    ItemValue::Binary(b) => String::from_utf8_lossy(b).into_owned(),
                };
                find_uuid(&v)
            })
        };
        // Every tag in the file, not just the primary one.
        recording_mbid = file.tags().iter().find_map(find_mbid);
        if let Some(tag) = file.primary_tag() {
            if let Some(a) = tag.artist() {
                artist = a.to_string();
            }
            if let Some(t) = tag.title() {
                title = t.to_string();
            }
        }
    }
    if recording_mbid.is_none() {
        // lofty's generic tag drops TXXX frames with descriptions it doesn't know, which is
        // exactly Explo's "MusicBrainz_TrackId", so look at the raw ID3v2 frames of an MP3.
        recording_mbid = std::fs::File::open(path)
            .ok()
            .and_then(|mut fh| MpegFile::read_from(&mut fh, ParseOptions::new().read_properties(false)).ok())
            .and_then(|mp3| {
                mp3.id3v2()?.into_iter().find_map(|frame| match frame {
                    Frame::UserText(t) if is_recording_id_key(&t.description) => find_uuid(&t.content),
                    _ => None,
                })
            });
    }
    Found { path: path.to_path_buf(), artist, title, recording_mbid, duration_ms }
}

fn is_recording_id_key(key: &str) -> bool {
    let k: String = key.to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    k.contains("musicbrainztrackid") || k.contains("musicbrainzrecordingid")
}

/// The first UUID anywhere in `s` (values may carry NULs, braces or a second value).
fn find_uuid(s: &str) -> Option<String> {
    (0..s.len().saturating_sub(35)).filter_map(|i| s.get(i..i + 36)).find(|w| is_uuid(w)).map(str::to_string)
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.chars()
            .enumerate()
            .all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { c == '-' } else { c.is_ascii_hexdigit() })
}

/// "Artist - Title" → ("Artist", "Title"); anything else is all title.
fn parse_stem(stem: &str) -> (String, String) {
    match stem.split_once(" - ") {
        Some((a, t)) => (a.trim().to_string(), t.trim().to_string()),
        None => (String::new(), stem.trim().to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_renders_and_sanitizes() {
        let t = Track {
            id: 1,
            recording_mbid: "x".into(),
            title: "AC/DC: Live?".into(),
            artist: "AC/DC feat. X".into(),
            album_artist: "AC/DC".into(),
            album: "Back in Black".into(),
            year: Some(1980),
            track_no: Some(3),
            path: None,
            status: "wanted".into(),
            source_url: None,
            error: None,
            artist_mbid: None,
            album_mbid: String::new(),
            album_artist_mbid: String::new(),
            duration_ms: None,
        };
        let p = render_template("{album_artist}/{album} ({year})/{track:02} - {title}.{ext}", &t, "opus").unwrap();
        assert_eq!(p, PathBuf::from("AC_DC/Back in Black (1980)/03 - AC_DC - Live.opus"));
        assert_eq!(sanitize("POST HUMAN: NeX GEn"), "POST HUMAN - NeX GEn");
        assert_eq!(sanitize("R U Mine?"), "R U Mine");
        assert_eq!(sanitize("..."), "_");
        assert_eq!(parse_stem("Bicep - Apricots"), ("Bicep".into(), "Apricots".into()));
        assert_eq!(parse_stem("Apricots"), (String::new(), "Apricots".into()));
        assert!(is_uuid("5dcf0f17-d13b-47c1-8386-fea1f5e25875"));
        assert!(!is_uuid("not-an-id"));
        assert_eq!(
            find_uuid("{5dcf0f17-d13b-47c1-8386-fea1f5e25875}\0"),
            Some("5dcf0f17-d13b-47c1-8386-fea1f5e25875".into())
        );
        assert_eq!(find_uuid("5dcf0f17"), None);
        assert!(render_template("{nope}", &t, "opus").is_err());
        assert!(validate_template("{artist}/{title}.{ext}").is_ok());
    }

    /// Explo's files: ID3v2.4, a UTF-8 TXXX frame described "MusicBrainz_TrackId" whose value
    /// ends in a NUL. Written by ffmpeg exactly like Explo does it.
    #[test]
    fn reads_explo_txxx_recording_id() {
        let found = scan(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")).unwrap();
        let f = found.iter().find(|f| f.path.ends_with("explo-txxx.mp3")).unwrap();
        assert_eq!(f.recording_mbid.as_deref(), Some("d6b2826c-d46d-4650-b011-44dcbb53ab06"));
        assert_eq!((f.artist.as_str(), f.title.as_str()), ("Clean Bandit", "Solo feat. Demi Lovato"));
    }
}
