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
    // lofty's generic tag never sees TXXX frames it has no key for (Explo's
    // "MusicBrainz_TrackId", "MusicBrainz_AlbumId"), so `clear()` below would leave them next
    // to ours with stale values. Drop every TXXX frame from the raw ID3v2 tag first; the ones
    // that matter are written again from `t` a moment later.
    if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("mp3"))
        && let Ok(mut fh) = std::fs::OpenOptions::new().read(true).write(true).open(path)
        && let Ok(mut mp3) = MpegFile::read_from(&mut fh, ParseOptions::new().read_properties(false))
        && let Some(id3) = mp3.id3v2_mut()
    {
        id3.remove(&lofty::id3::v2::FrameId::Valid(std::borrow::Cow::Borrowed("TXXX"))).for_each(drop);
        mp3.save_to(&mut fh, WriteOptions::default())
            .with_context(|| format!("rewriting tags of {}", path.display()))?;
    }
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

/// Put a file at its templated place (moved, or copied with `copy`) and tag it there. The
/// source is only ever read when copying, and never modified before the move succeeds. Returns
/// the path relative to `library`.
pub fn file_track(library: &Path, template: &str, t: &Track, src: &Path, copy: bool) -> Result<PathBuf> {
    let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("bin");
    let rel = unique(library, &render_template(template, t, ext)?);
    let dest = library.join(&rel);
    std::fs::create_dir_all(dest.parent().context("template has no directory part")?)?;
    if copy {
        std::fs::copy(src, &dest).with_context(|| format!("copying {} → {}", src.display(), dest.display()))?;
        // `copy` keeps the source's mode; a read-only source would make the copy untaggable.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o644))?;
    } else {
        move_file(src, &dest)?;
    }
    if let Err(e) = tag(&dest, t) {
        // Leave things as they were: the copy goes, a moved file goes back.
        if copy {
            let _ = std::fs::remove_file(&dest);
        } else {
            let _ = move_file(&dest, src);
        }
        prune(library, dest.parent());
        return Err(e);
    }
    Ok(rel)
}

/// Never overwrite: two recordings can render the same path ("End of You" twice on one
/// single). The second gets " (2)", " (3)", … before the extension.
pub fn unique(library: &Path, rel: &Path) -> PathBuf {
    if !library.join(rel).exists() {
        return rel.to_path_buf();
    }
    let stem = rel.file_stem().and_then(|s| s.to_str()).unwrap_or("track");
    let ext = rel.extension().and_then(|e| e.to_str()).map(|e| format!(".{e}")).unwrap_or_default();
    (2..)
        .map(|n| rel.with_file_name(format!("{stem} ({n}){ext}")))
        .find(|p| !library.join(p).exists())
        .expect("an unused name exists")
}

/// Do two files hold the same audio, as far as length tells (within a second)?
pub fn same_audio(a: &Path, b: &Path) -> bool {
    let len = |p: &Path| {
        Probe::open(p)
            .and_then(|p| p.options(ParseOptions::new().read_tags(false)).read())
            .ok()
            .map(|f| f.properties().duration())
    };
    match (len(a), len(b)) {
        (Some(x), Some(y)) => x.abs_diff(y) <= std::time::Duration::from_secs(1),
        _ => false,
    }
}

/// Can this file be modified and renamed by us? Opening for append is the cheap test that
/// matches what tagging and moving need.
pub fn writable(path: &Path) -> bool {
    std::fs::OpenOptions::new().append(true).open(path).is_ok()
}

/// Move a track already in the library to a new relative path and drop directories left empty.
pub fn move_track(library: &Path, old_rel: &Path, new_rel: &Path) -> Result<()> {
    let (from, to) = (library.join(old_rel), library.join(new_rel));
    std::fs::create_dir_all(to.parent().context("template has no directory part")?)?;
    move_file(&from, &to)?;
    prune(library, from.parent());
    Ok(())
}

/// Delete a track's file (if it is still there) and drop directories left empty.
pub fn remove_track(library: &Path, rel: &Path) -> Result<()> {
    let file = library.join(rel);
    match std::fs::remove_file(&file) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("deleting {}", file.display())),
    }
    prune(library, file.parent());
    Ok(())
}

/// Remove `dir` and its parents while they are empty, stopping at the library root.
fn prune(library: &Path, mut dir: Option<&Path>) {
    while let Some(d) = dir
        && d != library
        && std::fs::remove_dir(d).is_ok()
    {
        dir = d.parent();
    }
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
    pub size: i64,
    /// Seconds since the epoch; with `size` it tells whether a cached lookup still applies.
    pub mtime: i64,
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

pub fn inspect(path: &Path) -> Found {
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
    let meta = std::fs::metadata(path).ok();
    let size = meta.as_ref().map(|m| m.len() as i64).unwrap_or(0);
    let mtime = meta
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Found { path: path.to_path_buf(), size, mtime, artist, title, recording_mbid, duration_ms }
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
        let dir = std::env::temp_dir().join(format!("td-unique-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("A")).unwrap();
        std::fs::write(dir.join("A/01 - X.opus"), b"").unwrap();
        std::fs::write(dir.join("A/01 - X (2).opus"), b"").unwrap();
        assert_eq!(unique(&dir, Path::new("A/01 - X.opus")), PathBuf::from("A/01 - X (3).opus"));
        assert_eq!(unique(&dir, Path::new("A/02 - Y.opus")), PathBuf::from("A/02 - Y.opus"));
        let _ = std::fs::remove_dir_all(&dir);
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
