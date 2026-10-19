pub mod api;
pub mod app;
pub mod cli;
pub mod config;
pub mod daemon;
pub mod db;
pub mod download;
pub mod external;
pub mod library;
pub mod listenbrainz;
pub mod m3u;
pub mod musicbrainz;
pub mod player;
pub mod render;
pub mod request;
pub mod steam;
pub mod subsonic;
pub mod sync;
pub mod telegram;
pub mod theme;
pub mod tui;

pub use app::{App, Event};

pub const USER_AGENT: &str = concat!("tapedeck/", env!("CARGO_PKG_VERSION"), " (", env!("CARGO_PKG_REPOSITORY"), ")");

/// One HTTP client for the odd outbound fetch (cover art, the Steam store); the API clients
/// that talk a lot (ListenBrainz, MusicBrainz, Telegram) keep their own.
pub(crate) fn http() -> &'static reqwest::Client {
    static HTTP: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    HTTP.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .expect("reqwest client")
    })
}
