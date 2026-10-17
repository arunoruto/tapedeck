//! Where playlists go. Targets only ever see a title and absolute file paths.

use std::path::PathBuf;

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde::Serialize;

use crate::config::Config;

#[derive(Serialize, Debug, Clone)]
pub struct RemotePlaylist {
    pub id: String,
    pub title: String,
    pub tracks: i64,
    /// Server-generated (Plex smart playlists); never bulk-deleted unless asked.
    pub smart: bool,
}

pub mod plex;

#[async_trait]
pub trait SyncTarget: Send + Sync {
    fn name(&self) -> &'static str;
    /// Create or replace the playlist `title` with these local files. Returns the remote id.
    async fn sync_playlist(&self, title: &str, files: &[PathBuf]) -> Result<String>;
    async fn list_playlists(&self) -> Result<Vec<RemotePlaylist>>;
    async fn delete_playlist(&self, id: &str) -> Result<()>;
}

/// `*` matches any run of characters, `?` one character. Enough for "Weekly-Jams-*".
pub fn glob_match(pattern: &str, s: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), s.chars().collect());
    fn go(p: &[char], t: &[char]) -> bool {
        match (p.first(), t.first()) {
            (None, None) => true,
            (Some('*'), _) => go(&p[1..], t) || (!t.is_empty() && go(p, &t[1..])),
            (Some('?'), Some(_)) => go(&p[1..], &t[1..]),
            (Some(a), Some(b)) if a == b => go(&p[1..], &t[1..]),
            _ => false,
        }
    }
    go(&p, &t)
}

pub fn from_config(cfg: &Config) -> Result<Vec<Box<dyn SyncTarget>>> {
    cfg.sync
        .targets
        .iter()
        .map(|t| -> Result<Box<dyn SyncTarget>> {
            match t.as_str() {
                "plex" => Ok(Box::new(plex::Plex::new(cfg.sync.plex.clone().expect("validated in Config::load")))),
                other => bail!("unknown sync target {other:?}"),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn glob() {
        use super::glob_match;
        assert!(glob_match("Weekly-Jams-*", "Weekly-Jams-2026-Week41"));
        assert!(glob_match("*Jams*", "Daily-Jams"));
        assert!(glob_match("Weekly Jams", "Weekly Jams"));
        assert!(glob_match("Week?y", "Weekly"));
        assert!(!glob_match("Weekly-Jams-*", "Weekly-Jams"));
        assert!(!glob_match("Weekly Jams", "Weekly Jams (tapedeck)"));
    }
}
