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
    /// TUI theme: a built-in name or `<config dir>/themes/<name>.toml`.
    #[serde(default = "default_theme")]
    pub theme: String,
    /// Folder the config file was read from; themes live in its `themes/` subfolder.
    #[serde(skip)]
    pub config_dir: PathBuf,
    pub listenbrainz: ListenBrainz,
    #[serde(default)]
    pub download: Download,
    #[serde(default)]
    pub player: PlayerCfg,
    #[serde(default)]
    pub sync: Sync,
    #[serde(default)]
    pub external: External,
    /// Song requests over Telegram; off unless a token is set.
    #[serde(default)]
    pub telegram: Telegram,
    /// The daemon's HTTP API for `td` on other machines; off unless `listen` is set.
    #[serde(default)]
    pub server: Server,
    /// Use another machine's daemon instead of a local database and socket.
    #[serde(default)]
    pub remote: Remote,
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct Server {
    /// `host:port`, e.g. a Tailscale address. Nothing is served without it.
    pub listen: Option<String>,
    /// Admin token: with it a client may run everything. Without one configured, only the
    /// anonymous scopes are reachable over HTTP at all.
    pub token: Option<String>,
    pub token_file: Option<PathBuf>,
    /// What a client without the token may do: "read" (status, search, listings), "stream"
    /// (the audio), "listen" (scrobbles), plus optionally "request" and "sync".
    #[serde(default = "default_anonymous")]
    pub anonymous: Vec<String>,
    /// Sign-in for Subsonic clients on `/rest/`.
    #[serde(default)]
    pub subsonic: Subsonic,
}

/// `[server.subsonic]`. Without `user`, every Subsonic client is anonymous and gets the
/// `anonymous` scopes, enough on a LAN. With `user` and `password`/`password_file`, clients must
/// sign in (the protocol's salted-MD5 token or the password itself) and then may do everything.
#[derive(Deserialize, Clone, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct Subsonic {
    pub user: Option<String>,
    pub password: Option<String>,
    pub password_file: Option<PathBuf>,
}

fn default_anonymous() -> Vec<String> {
    ["read", "stream", "listen"].map(String::from).to_vec()
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct Remote {
    /// `http://host:port` of a daemon with `[server]` set.
    pub url: Option<String>,
    pub token: Option<String>,
    pub token_file: Option<PathBuf>,
}

/// Playback in the TUI, through mpv.
#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct PlayerCfg {
    #[serde(default = "default_mpv")]
    pub mpv_bin: String,
    /// Report plays to ListenBrainz (through the daemon, which holds the token).
    #[serde(default = "default_true")]
    pub scrobble: bool,
}

impl Default for PlayerCfg {
    fn default() -> Self {
        Self { mpv_bin: default_mpv(), scrobble: true }
    }
}

fn default_mpv() -> String {
    "mpv".into()
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct Telegram {
    pub token: Option<String>,
    pub token_file: Option<PathBuf>,
    /// Telegram user ids that may send requests; everyone else is ignored.
    #[serde(default)]
    pub allowed_users: Vec<i64>,
}

/// Read-only libraries managed by something else (Lidarr). Recordings found there are never
/// downloaded or kept twice; nothing under these paths is ever written.
#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct External {
    #[serde(default)]
    pub paths: Vec<PathBuf>,
    /// How often the daemon rescans them; "0" turns the periodic scan off (sync still scans).
    #[serde(default = "default_scan_interval")]
    pub scan_interval: String,
    /// When an external copy of an own track appears: "auto" repoints and deletes the own file,
    /// "ask" only reports it (`td external dedupe` acts), "never" ignores it.
    #[serde(default = "default_prefer_external")]
    pub prefer_external: String,
}

impl Default for External {
    fn default() -> Self {
        Self { paths: vec![], scan_interval: default_scan_interval(), prefer_external: default_prefer_external() }
    }
}

fn default_scan_interval() -> String {
    "1h".into()
}
fn default_prefer_external() -> String {
    "ask".into()
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
    /// Whether the daemon syncs right when it starts (and after every restart), or waits one
    /// interval first.
    #[serde(default = "default_true")]
    pub on_start: bool,
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
        Self {
            interval: default_interval(),
            targets: vec![],
            playlist_title: default_playlist_title(),
            on_start: true,
            plex: None,
        }
    }
}

fn default_theme() -> String {
    crate::theme::DEFAULT.into()
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
fn default_true() -> bool {
    true
}
fn default_playlist_title() -> String {
    "{name}".into()
}

impl Default for Download {
    fn default() -> Self {
        Self { backend: default_backend(), format: default_format(), ytdlp_bin: default_ytdlp(), ffmpeg_bin: None }
    }
}

/// `TAPEDECK_<KEY>` and `TAPEDECK_<SECTION>__<KEY>` (one `__` per nesting level, any case) carry
/// config keys, so a container can be configured, secrets included, without a file.
fn is_env_key(name: &str) -> bool {
    name.strip_prefix("TAPEDECK_").is_some_and(|rest| !rest.is_empty() && rest != "CONFIG")
}

/// Lay the `TAPEDECK_*` variables over the parsed file; the environment wins. `true`/`false`
/// and values starting with `[`, `{` or `"` are read as TOML, everything else is a string, and
/// an empty value removes the key (how to switch off a default the image sets).
pub fn apply_env(table: &mut toml::Table, vars: impl Iterator<Item = (String, String)>) {
    for (name, raw) in vars.filter(|(k, _)| is_env_key(k)) {
        let path = &name["TAPEDECK_".len()..];
        let (parents, last) = path.rsplit_once("__").unwrap_or(("", path));
        let last = last.to_lowercase();
        let mut node = &mut *table;
        for key in parents.split("__").filter(|k| !k.is_empty()) {
            let slot = node.entry(key.to_lowercase()).or_insert_with(|| toml::Value::Table(toml::Table::new()));
            if !slot.is_table() {
                *slot = toml::Value::Table(toml::Table::new());
            }
            node = slot.as_table_mut().expect("just made a table");
        }
        if raw.is_empty() {
            node.remove(&last);
        } else {
            node.insert(last, env_value(&raw));
        }
    }
}

fn env_value(raw: &str) -> toml::Value {
    let literal = matches!(raw, "true" | "false") || raw.starts_with(['[', '{', '"']);
    literal
        .then(|| toml::from_str::<toml::Table>(&format!("v = {raw}")).ok())
        .flatten()
        .and_then(|mut t| t.remove("v"))
        .unwrap_or_else(|| toml::Value::String(raw.to_string()))
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let path = match path {
            Some(p) => p.to_path_buf(),
            None => xdg("XDG_CONFIG_HOME", ".config").join("tapedeck/config.toml"),
        };
        let mut table: toml::Table = match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
            // No file is fine when the environment carries the config (containers).
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && std::env::vars().any(|(k, _)| is_env_key(&k)) => {
                toml::Table::new()
            }
            Err(e) => {
                return Err(e).with_context(|| format!("reading config {} (see config.example.toml)", path.display()));
            }
        };
        apply_env(&mut table, std::env::vars());
        let mut cfg = Self::parse_table(table)?;
        cfg.config_dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        // Secrets are read later, by whoever actually talks to a target (`resolve_secrets`):
        // a group member running `td status` against the daemon must not need the token file.
        Ok(cfg)
    }

    /// Parse and validate, without touching the filesystem or the environment.
    pub fn parse(text: &str) -> Result<Self> {
        Self::parse_table(toml::from_str(text).context("parsing config")?)
    }

    fn parse_table(table: toml::Table) -> Result<Self> {
        let cfg: Config = table.try_into().context("parsing config")?;
        crate::library::validate_template(&cfg.path_template)?;
        for scope in &cfg.server.anonymous {
            if !matches!(scope.as_str(), "read" | "stream" | "listen" | "request" | "sync") {
                bail!("[server] anonymous: unknown scope {scope:?} (read, stream, listen, request, sync)");
            }
        }
        let sub = &cfg.server.subsonic;
        if sub.user.is_some() && sub.password.is_none() && sub.password_file.is_none() {
            bail!("[server.subsonic] user needs password or password_file");
        }
        if sub.user.is_none() && (sub.password.is_some() || sub.password_file.is_some()) {
            bail!("[server.subsonic] password needs user");
        }
        if !matches!(cfg.external.prefer_external.as_str(), "auto" | "ask" | "never") {
            bail!("external.prefer_external must be auto, ask or never");
        }
        parse_duration(&cfg.external.scan_interval, "external.scan_interval")?;
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
    /// Turn `token_file`s into tokens. Called by the daemon, by in-process commands and by the
    /// TUI's remote listing, not at load time, so the files can stay readable by the service
    /// user only.
    pub fn resolve_secrets(&mut self) -> Result<()> {
        read_token(&mut self.listenbrainz.token, &self.listenbrainz.token_file, "[listenbrainz]")?;
        read_token(&mut self.telegram.token, &self.telegram.token_file, "[telegram]")?;
        read_token(&mut self.server.token, &self.server.token_file, "[server]")?;
        read_token(
            &mut self.server.subsonic.password,
            &self.server.subsonic.password_file,
            "[server.subsonic] password",
        )?;
        read_token(&mut self.remote.token, &self.remote.token_file, "[remote]")?;
        if let Some(p) = &mut self.sync.plex {
            read_token(&mut p.token, &p.token_file, "[sync.plex]")?;
        }
        Ok(())
    }

    pub fn playlist_dir(&self) -> PathBuf {
        self.playlist_dir.clone().unwrap_or_else(|| self.library.join("playlists"))
    }

    pub fn themes_dir(&self) -> PathBuf {
        self.config_dir.join("themes")
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
        parse_duration(&self.sync.interval, "sync.interval")
    }

    /// Zero means no periodic scan.
    pub fn external_scan_interval(&self) -> Result<Duration> {
        parse_duration(&self.external.scan_interval, "external.scan_interval")
    }
}

/// "6h", "30m", "90s", "1d"; a bare "0" is allowed and means off.
fn parse_duration(value: &str, key: &str) -> Result<Duration> {
    let s = value.trim();
    if s == "0" {
        return Ok(Duration::ZERO);
    }
    let (num, unit) = s.split_at(s.len().saturating_sub(1));
    let n: u64 = num.parse().with_context(|| format!("bad {key} {s:?}, want e.g. 6h or 30m"))?;
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => bail!("bad {key} unit in {s:?}"),
    };
    Ok(Duration::from_secs(n * mult))
}

/// `what` is "[section]" for a token, "[section] password" for anything else.
fn read_token(token: &mut Option<String>, file: &Option<PathBuf>, what: &str) -> Result<()> {
    let (section, key) = what.split_once(' ').unwrap_or((what, "token"));
    match (&token, file) {
        (Some(_), Some(_)) => bail!("{section}: set {key} or {key}_file, not both"),
        (None, Some(f)) => {
            let text =
                std::fs::read_to_string(f).with_context(|| format!("{section}: reading {key}_file {}", f.display()))?;
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
    fn env_lays_over_the_file() {
        let mut t: toml::Table =
            toml::from_str("library = \"/m\"\nplaylist_dir = \"/p\"\n[listenbrainz]\nuser = \"u\"\n").unwrap();
        let vars = [
            ("TAPEDECK_LIBRARY", "/music"),
            ("TAPEDECK_PLAYLIST_DIR", ""),
            ("TAPEDECK_LISTENBRAINZ__TOKEN", "abc"),
            ("TAPEDECK_listenbrainz__playlists", "[\"weekly-jams\"]"),
            ("TAPEDECK_SERVER__LISTEN", "0.0.0.0:8687"),
            ("TAPEDECK_SYNC__PLEX__TOKEN_FILE", "/run/secrets/plex"),
            ("TAPEDECK_SYNC__PLEX__URL", "http://plex:32400"),
            ("TAPEDECK_SYNC__PLEX__SECTION", "Music"),
            ("TAPEDECK_SYNC__ON_START", "true"),
            ("TAPEDECK_CONFIG", "/x"),
            ("TAPEDECK_", "nope"),
            ("HOME", "/h"),
        ];
        apply_env(&mut t, vars.into_iter().map(|(k, v)| (k.to_string(), v.to_string())));
        let cfg = Config::parse_table(t).unwrap();
        assert_eq!(cfg.library, PathBuf::from("/music"));
        assert_eq!(cfg.playlist_dir(), PathBuf::from("/music/playlists"), "empty value removes the key");
        assert_eq!(cfg.listenbrainz.token.as_deref(), Some("abc"));
        assert_eq!(cfg.listenbrainz.playlists, ["weekly-jams"]);
        assert_eq!(cfg.server.listen.as_deref(), Some("0.0.0.0:8687"));
        let plex = cfg.sync.plex.unwrap();
        assert_eq!(plex.token_file, Some(PathBuf::from("/run/secrets/plex")));
        assert!(cfg.sync.on_start);
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
