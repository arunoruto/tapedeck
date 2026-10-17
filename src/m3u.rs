//! One m3u8 per fetched ListenBrainz playlist, so the library stays usable without tapedeck.

use std::path::{Component, Path, PathBuf};

use anyhow::Result;

use crate::db::Track;

/// Write `<playlist_dir>/<stem>.m3u8` listing the ready tracks. Returns the file path.
pub fn write(playlist_dir: &Path, library: &Path, stem: &str, title: &str, tracks: &[Track]) -> Result<PathBuf> {
    std::fs::create_dir_all(playlist_dir)?;
    let mut body = format!("#EXTM3U\n#PLAYLIST:{title}\n");
    for t in tracks.iter().filter(|t| t.status == "ready") {
        let Some(rel) = &t.path else { continue };
        let entry = relative_to(playlist_dir, &library.join(rel));
        let secs = t.duration_ms.map(|ms| (ms + 500) / 1000).unwrap_or(-1);
        body.push_str(&format!("#EXTINF:{secs},{} - {}\n{}\n", t.artist, t.title, entry.display()));
    }
    let file = playlist_dir.join(format!("{}.m3u8", safe_name(stem)));
    std::fs::write(&file, body)?;
    Ok(file)
}

/// Plain m3u8 with absolute paths, for backups of playlists that live elsewhere.
pub fn write_entries(file: &Path, title: &str, entries: &[(String, PathBuf)]) -> Result<()> {
    let mut body = format!("#EXTM3U\n#PLAYLIST:{title}\n");
    for (label, path) in entries {
        body.push_str(&format!("#EXTINF:-1,{label}\n{}\n", path.display()));
    }
    std::fs::write(file, body)?;
    Ok(())
}

/// Archive file stem for a playlist: the ISO week for weekly kinds (`2026-w41`), the date for
/// daily ones (`2026-10-17`). Falls back to the date when it cannot be parsed.
pub fn period(kind: &str, date: &str) -> String {
    if kind.starts_with("daily") {
        return date.to_string();
    }
    let mut parts = date.splitn(3, '-').map(|p| p.parse::<i64>());
    match (parts.next(), parts.next(), parts.next()) {
        (Some(Ok(y)), Some(Ok(m)), Some(Ok(d))) if (1..=12).contains(&m) && (1..=31).contains(&d) => {
            let (year, week) = iso_week(y, m, d);
            format!("{year}-w{week:02}")
        }
        _ => date.to_string(),
    }
}

/// ISO 8601 week (year, week) of a civil date; weeks start Monday, week 1 holds January 4th.
fn iso_week(y: i64, m: i64, d: i64) -> (i64, i64) {
    // Days since 1970-01-01, Howard Hinnant's days_from_civil.
    fn days(y: i64, m: i64, d: i64) -> i64 {
        let y = if m <= 2 { y - 1 } else { y };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146097 + doe - 719468
    }
    let weekday = (days(y, m, d) + 3).rem_euclid(7); // Monday = 0
    let ordinal = days(y, m, d) - days(y, 1, 1) + 1;
    let weeks_in = |y: i64| {
        let jan1 = (days(y, 1, 1) + 3).rem_euclid(7);
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        if jan1 == 3 || (leap && jan1 == 2) { 53 } else { 52 }
    };
    let week = (ordinal - (weekday + 1) + 10) / 7;
    if week < 1 {
        (y - 1, weeks_in(y - 1))
    } else if week > weeks_in(y) {
        (y + 1, 1)
    } else {
        (y, week)
    }
}

/// Lowercase, dash-separated file name: "Weekly-Jams-2026-Week22 [4545]" → "weekly-jams-2026-week22-4545".
pub fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars().flat_map(|c| c.to_lowercase()) {
        if c.is_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

pub fn safe_name(s: &str) -> String {
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
        assert_eq!(
            relative_to(Path::new("/m/Playlists/archive/Weekly Jams"), Path::new("/m/A/B/01.opus")),
            PathBuf::from("../../../A/B/01.opus")
        );
    }

    #[test]
    fn periods() {
        assert_eq!(period("weekly-jams", "2026-10-05"), "2026-w41");
        assert_eq!(period("weekly-exploration", "2026-10-17"), "2026-w42");
        assert_eq!(period("weekly-jams", "2021-01-03"), "2020-w53");
        assert_eq!(period("weekly-jams", "2024-12-30"), "2025-w01");
        assert_eq!(period("weekly-jams", "2020-12-31"), "2020-w53");
        assert_eq!(period("daily-jams", "2026-10-17"), "2026-10-17");
        assert_eq!(period("weekly-jams", "undated"), "undated");
        assert_eq!(slug("Weekly-Jams-2026-Week22 [4545]"), "weekly-jams-2026-week22-4545");
        assert_eq!(slug("Måneskin & Co: Rush!"), "måneskin-co-rush");
    }
}
