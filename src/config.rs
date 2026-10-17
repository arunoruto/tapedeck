use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Root of the music library. Downloads are filed under here.
    pub library: PathBuf,
    /// Lidarr-style path template, relative to `library`.
    /// Tokens: {artist} {album_artist} {album} {year} {track} {track:02} {title} {ext}
    #[serde(default = "default_template")]
    pub path_template: String,
    /// Where m3u8 files go. Default: `<library>/Playlists`.
    pub playlist_dir: Option<PathBuf>,
    /// SQLite file. Default: `$XDG_DATA_HOME/tapedeck/tapedeck.db`.
    pub database: Option<PathBuf>,
    /// Daemon socket. Default: `$XDG_RUNTIME_DIR/tapedeck.sock`.
    pub socket: Option<PathBuf>,
    pub listenbrainz: ListenBrainz,
    #[serde(default)]
    pub download: Download,
    #[serde(default)]
    pub sync: Sync,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct ListenBrainz {
    pub user: String,
    /// Only needed for private playlists. Inline, or read from `token_file`.
    pub token: Option<String>,
    pub token_file: Option<PathBuf>,
    /// `source_patch` names of the created-for playlists to follow.
    #[serde(default = "default_playlists")]
    pub playlists: Vec<String>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Download {
    #[serde(default = "default_backend")]
    pub backend: String,
    /// Audio format passed to yt-dlp's --audio-format.
    #[serde(default = "default_format")]
    pub format: String,
    #[serde(default = "default_ytdlp")]
    pub ytdlp_bin: String,
    /// Passed to yt-dlp as --ffmpeg-location. Default: whatever ffmpeg yt-dlp finds on PATH.
    pub ffmpeg_bin: Option<String>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Sync {
    /// How often the daemon runs `sync`, e.g. "6h", "30m".
    #[serde(default = "default_interval")]
    pub interval: String,
    /// Enabled targets, in order.
    #[serde(default)]
    pub targets: Vec<String>,
    /// Playlist name on the targets. `{name}` is "Weekly Jams", `{kind}` is "weekly-jams".
    #[serde(default = "default_playlist_title")]
    pub playlist_title: String,
    pub plex: Option<Plex>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Plex {
    pub url: String,
    /// X-Plex-Token. Set this or `token_file` (a path holding the token, e.g. from sops-nix).
    pub token: Option<String>,
    pub token_file: Option<PathBuf>,
    /// Library section name, e.g. "Music".
    pub section: String,
    /// Local path prefix → path prefix as Plex sees it. Empty if identical.
    #[serde(default)]
    pub path_map: BTreeMap<String, String>,
}

// A missing [sync] table must mean the same as an empty one, so defaults live in one place.
impl Default for Sync {
    fn default() -> Self {
        Self { interval: default_interval(), targets: vec![], playlist_title: default_playlist_title(), plex: None }
    }
}

fn default_template() -> String {
    "{album_artist}/{album} ({year})/{track:02} - {title}.{ext}".into()
}
fn default_playlists() -> Vec<String> {
    vec!["weekly-jams".into(), "weekly-exploration".into()]
}
fn default_backend() -> String {
    "ytdlp".into()
}
fn default_format() -> String {
    "opus".into()
}
fn default_ytdlp() -> String {
    "yt-dlp".into()
}
fn default_interval() -> String {
    "6h".into()
}
fn default_playlist_title() -> String {
    "{name}".into()
}

impl Default for Download {
    fn default() -> Self {
        Self { backend: default_backend(), format: default_format(), ytdlp_bin: default_ytdlp(), ffmpeg_bin: None }
    }
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let path = match path {
            Some(p) => p.to_path_buf(),
            None => xdg("XDG_CONFIG_HOME", ".config").join("tapedeck/config.toml"),
        };
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading config {} (see config.example.toml)", path.display()))?;
        let mut cfg = Self::parse(&text)?;
        cfg.resolve_secrets()?;
        Ok(cfg)
    }

    /// Parse and validate, without touching the filesystem.
    pub fn parse(text: &str) -> Result<Self> {
        let cfg: Config = toml::from_str(text).context("parsing config")?;
        crate::library::validate_template(&cfg.path_template)?;
        for t in &cfg.sync.targets {
            if t == "plex" && cfg.sync.plex.is_none() {
                bail!("sync.targets lists plex but [sync.plex] is missing");
            }
        }
        if let Some(p) = &cfg.sync.plex
            && p.token.is_none()
            && p.token_file.is_none()
        {
            bail!("[sync.plex] needs token or token_file");
        }
        cfg.sync_interval()?;
        Ok(cfg)
    }

    /// Replace every `token_file` with the file's contents so the rest of the code only sees `token`.
    pub fn resolve_secrets(&mut self) -> Result<()> {
        read_token(&mut self.listenbrainz.token, &self.listenbrainz.token_file, "[listenbrainz]")?;
        if let Some(p) = &mut self.sync.plex {
            read_token(&mut p.token, &p.token_file, "[sync.plex]")?;
        }
        Ok(())
    }

    pub fn playlist_dir(&self) -> PathBuf {
        self.playlist_dir.clone().unwrap_or_else(|| self.library.join("playlists"))
    }

    pub fn database_path(&self) -> PathBuf {
        self.database.clone().unwrap_or_else(|| xdg("XDG_DATA_HOME", ".local/share").join("tapedeck/tapedeck.db"))
    }

    pub fn socket_path(&self) -> PathBuf {
        self.socket.clone().unwrap_or_else(|| match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(d) => PathBuf::from(d).join("tapedeck.sock"),
            None => xdg("XDG_DATA_HOME", ".local/share").join("tapedeck/tapedeck.sock"),
        })
    }

    /// Scratch dir for in-flight downloads. Inside the library so the final move is a rename.
    pub fn tmp_dir(&self) -> PathBuf {
        self.library.join(".tapedeck-tmp")
    }

    pub fn sync_interval(&self) -> Result<Duration> {
        let s = self.sync.interval.trim();
        let (num, unit) = s.split_at(s.len().saturating_sub(1));
        let n: u64 = num.parse().with_context(|| format!("bad sync.interval {s:?}, want e.g. 6h or 30m"))?;
        let mult = match unit {
            "s" => 1,
            "m" => 60,
            "h" => 3600,
            "d" => 86400,
            _ => bail!("bad sync.interval unit in {s:?}"),
        };
        Ok(Duration::from_secs(n * mult))
    }
}

fn read_token(token: &mut Option<String>, file: &Option<PathBuf>, section: &str) -> Result<()> {
    match (&token, file) {
        (Some(_), Some(_)) => bail!("{section}: set token or token_file, not both"),
        (None, Some(f)) => {
            let text =
                std::fs::read_to_string(f).with_context(|| format!("{section}: reading token_file {}", f.display()))?;
            *token = Some(text.trim().to_string());
        }
        _ => {}
    }
    Ok(())
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::home_dir().expect("HOME not set").join(fallback))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses_and_defaults_apply() {
        let cfg = Config::parse(include_str!("../config.example.toml")).unwrap();
        assert_eq!(cfg.playlist_dir(), PathBuf::from("/srv/music/playlists"));
        assert_eq!(cfg.sync_interval().unwrap(), Duration::from_secs(6 * 3600));
        assert_eq!(cfg.download.format, "opus");
        assert_eq!(cfg.sync.targets, ["plex"]);
    }

    #[test]
    fn rejects_bad_config() {
        let base = "library = \"/m\"\n[listenbrainz]\nuser = \"u\"\n";
        assert!(Config::parse(base).is_ok());
        assert!(Config::parse(&format!("{base}[sync]\ntargets = [\"plex\"]\n")).is_err(), "plex without [sync.plex]");
        assert!(Config::parse(&format!("{base}[sync]\ninterval = \"soon\"\n")).is_err());
        assert!(Config::parse(&format!("{base}typo = 1\n")).is_err(), "unknown keys are errors");
        assert!(
            Config::parse("path_template = \"{title}\"\n[listenbrainz]\nuser = \"u\"\n").is_err(),
            "library required"
        );
        let plex = "[sync.plex]\nurl = \"http://p\"\nsection = \"Music\"\n";
        assert!(Config::parse(&format!("{base}{plex}")).is_err(), "plex needs a token");
        assert!(Config::parse(&format!("{base}{plex}token_file = \"/run/secrets/plex\"\n")).is_ok());
    }

    #[test]
    fn token_file_is_read_and_trimmed() {
        let file = std::env::temp_dir().join(format!("tapedeck-token-{}", std::process::id()));
        std::fs::write(&file, "s3cret\n").unwrap();
        let text = format!(
            "library = \"/m\"\n[listenbrainz]\nuser = \"u\"\ntoken_file = {file:?}\n[sync.plex]\nurl = \"http://p\"\nsection = \"Music\"\ntoken_file = {file:?}\n"
        );
        let mut cfg = Config::parse(&text).unwrap();
        cfg.resolve_secrets().unwrap();
        assert_eq!(cfg.listenbrainz.token.as_deref(), Some("s3cret"));
        assert_eq!(cfg.sync.plex.unwrap().token.as_deref(), Some("s3cret"));

        let mut both = Config::parse(&format!("{text}token = \"inline\"\n")).unwrap();
        assert!(both.resolve_secrets().unwrap_err().to_string().contains("not both"));
        let mut missing =
            Config::parse("library = \"/m\"\n[listenbrainz]\nuser = \"u\"\ntoken_file = \"/nonexistent\"\n").unwrap();
        assert!(missing.resolve_secrets().is_err());
        std::fs::remove_file(file).unwrap();
    }
}
