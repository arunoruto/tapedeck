//! One m3u8 per fetched ListenBrainz playlist, so the library stays usable without tapedeck.

use std::path::{Component, Path, PathBuf};

use anyhow::Result;

use crate::db::Track;

/// Write `<playlist_dir>/<name>.m3u8` listing the ready tracks. Returns the file path.
pub fn write(playlist_dir: &Path, library: &Path, name: &str, tracks: &[Track]) -> Result<PathBuf> {
    std::fs::create_dir_all(playlist_dir)?;
    let mut body = format!("#EXTM3U\n#PLAYLIST:{name}\n");
    for t in tracks.iter().filter(|t| t.status == "ready") {
        let Some(rel) = &t.path else { continue };
        let entry = relative_to(playlist_dir, &library.join(rel));
        body.push_str(&format!("#EXTINF:-1,{} - {}\n{}\n", t.artist, t.title, entry.display()));
    }
    let file = playlist_dir.join(format!("{}.m3u8", safe_name(name)));
    std::fs::write(&file, body)?;
    Ok(file)
}

fn safe_name(s: &str) -> String {
    s.chars().map(|c| if matches!(c, '/' | '\\' | ':' | '\0') { '_' } else { c }).collect::<String>().trim().to_string()
}

/// Path of `target` relative to `from` when they share a prefix, else `target` as-is.
fn relative_to(from: &Path, target: &Path) -> PathBuf {
    let from: Vec<Component> = from.components().collect();
    let target: Vec<Component> = target.components().collect();
    let common = from.iter().zip(&target).take_while(|(a, b)| a == b).count();
    if common == 0 {
        return target.iter().collect();
    }
    let mut out = PathBuf::new();
    for _ in common..from.len() {
        out.push("..");
    }
    for c in &target[common..] {
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths() {
        assert_eq!(
            relative_to(Path::new("/m/Playlists"), Path::new("/m/A/B/01.opus")),
            PathBuf::from("../A/B/01.opus")
        );
        assert_eq!(relative_to(Path::new("/x/pl"), Path::new("/m/A/01.opus")), PathBuf::from("../../m/A/01.opus"));
    }
}
