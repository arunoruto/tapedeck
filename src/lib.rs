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
pub mod render;
pub mod request;
pub mod sync;
pub mod telegram;
pub mod theme;
pub mod tui;

pub use app::{App, Event};

pub const USER_AGENT: &str = concat!("tapedeck/", env!("CARGO_PKG_VERSION"), " (", env!("CARGO_PKG_REPOSITORY"), ")");
