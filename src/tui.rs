//! `td tui` (and plain `td`): browse the library as artist › album › song, and the playlists
//! next to their tracks. Read-only for now: it opens the database directly, so it works
//! whether or not the daemon runs, and nothing here can break the library.

use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style, Stylize},
    text::Line,
    widgets::{Block, Cell, List, ListItem, ListState, Paragraph, Row, Table, TableState, Tabs},
};

use crate::{
    config::Config,
    db::{Db, Playlist, Track},
    sync::{self, RemotePlaylist},
};

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Library = 0,
    Playlists = 1,
}

const TABS: [&str; 2] = ["Library", "Playlists"];

/// What the playlists tab's left pane can point at.
enum Entry {
    Local(Playlist),
    Remote { target: &'static str, playlist: RemotePlaylist },
}

struct Tui {
    db: Db,
    tab: Tab,
    /// Focused pane within the tab, left to right.
    pane: usize,
    searching: bool,
    query: String,
    status: String,
    /// Library tab. The three columns are derived from `all` on every draw, see `shelf`.
    all: Vec<Track>,
    artists: ListState,
    albums: ListState,
    songs: TableState,
    /// Playlists tab.
    entries: Vec<Entry>,
    left: ListState,
    tracks: Vec<Track>,
    right: TableState,
}

/// The library tab's columns for one artist/album selection and filter.
struct Shelf<'a> {
    artists: Vec<(&'a str, usize)>,
    albums: Vec<(&'a str, Option<i64>, usize)>,
    songs: Vec<&'a Track>,
}

pub async fn run(config: Config) -> Result<()> {
    let db = Db::open(&config.database_path())?;
    // Remote lists are fetched once up front; everything after that is local and instant.
    let mut remote = vec![];
    for target in sync::from_config(&config)? {
        match target.list_playlists().await {
            Ok(list) => remote.extend(list.into_iter().map(|p| (target.name(), p))),
            Err(e) => tracing::warn!("{}: {e:#}", target.name()),
        }
    }
    let mut tui = Tui {
        db,
        tab: Tab::Library,
        pane: 0,
        searching: false,
        query: String::new(),
        status: String::new(),
        all: vec![],
        artists: ListState::default().with_selected(Some(0)),
        albums: ListState::default().with_selected(Some(0)),
        songs: TableState::default().with_selected(Some(0)),
        entries: vec![],
        left: ListState::default().with_selected(Some(0)),
        tracks: vec![],
        right: TableState::default().with_selected(Some(0)),
    };
    tui.reload(remote)?;
    tokio::task::spawn_blocking(move || {
        let mut terminal = ratatui::init();
        let result = tui.event_loop(&mut terminal);
        ratatui::restore();
        result
    })
    .await?
}

fn album_artist(t: &Track) -> &str {
    if t.album_artist.is_empty() { &t.artist } else { &t.album_artist }
}

fn matches(t: &Track, q: &str) -> bool {
    q.is_empty()
        || t.artist.to_lowercase().contains(q)
        || t.title.to_lowercase().contains(q)
        || t.album.to_lowercase().contains(q)
}

/// Group the filtered tracks by album artist, then album; `ai`/`bi` pick which groups to open.
/// A free function over `all` so callers can still borrow the list states mutably.
fn shelf<'a>(all: &'a [Track], query: &str, ai: usize, bi: usize) -> Shelf<'a> {
    let q = query.to_lowercase();
    let mut tracks: Vec<&Track> = all.iter().filter(|t| matches(t, &q)).collect();
    // ponytail: regrouped on every draw; cache it if libraries grow past tens of thousands of tracks
    tracks.sort_by_cached_key(|t| (album_artist(t).to_lowercase(), t.album.to_lowercase(), t.track_no));
    let same = |a: &str, b: &str| a.to_lowercase() == b.to_lowercase();
    let by_artist: Vec<&[&Track]> = tracks.chunk_by(|a, b| same(album_artist(a), album_artist(b))).collect();
    let artists = by_artist.iter().map(|g| (album_artist(g[0]), g.len())).collect();
    let ai = ai.min(by_artist.len().saturating_sub(1));
    let by_album: Vec<&[&Track]> =
        by_artist.get(ai).map(|g| g.chunk_by(|a, b| same(&a.album, &b.album)).collect()).unwrap_or_default();
    let albums = by_album.iter().map(|g| (g[0].album.as_str(), g[0].year, g.len())).collect();
    let bi = bi.min(by_album.len().saturating_sub(1));
    let songs = by_album.get(bi).map(|g| g.to_vec()).unwrap_or_default();
    Shelf { artists, albums, songs }
}

impl Tui {
    fn reload(&mut self, remote: Vec<(&'static str, RemotePlaylist)>) -> Result<()> {
        self.all = self.db.all_tracks()?;
        self.entries = self.db.playlists()?.into_iter().map(Entry::Local).collect();
        self.entries.extend(remote.into_iter().map(|(target, playlist)| Entry::Remote { target, playlist }));
        self.select_left(self.left.selected().unwrap_or(0).min(self.entries.len().saturating_sub(1)))
    }

    fn shelf(&self) -> Shelf<'_> {
        shelf(&self.all, &self.query, self.artists.selected().unwrap_or(0), self.albums.selected().unwrap_or(0))
    }

    fn select_left(&mut self, i: usize) -> Result<()> {
        self.left.select(Some(i));
        self.tracks = match self.entries.get(i) {
            Some(Entry::Local(p)) => self.db.playlist_tracks(p.id)?,
            _ => vec![],
        };
        self.right.select(Some(0));
        Ok(())
    }

    fn visible(&self) -> Vec<&Track> {
        let q = self.query.to_lowercase();
        self.tracks.iter().filter(|t| matches(t, &q)).collect()
    }

    fn selected_track(&self) -> Option<&Track> {
        match self.tab {
            Tab::Library => self.shelf().songs.get(self.songs.selected()?).copied(),
            Tab::Playlists => self.visible().get(self.right.selected()?).copied(),
        }
    }

    fn panes(&self) -> usize {
        match self.tab {
            Tab::Library => 3,
            Tab::Playlists => 2,
        }
    }

    fn set_tab(&mut self, tab: Tab) {
        self.tab = tab;
        self.pane = self.pane.min(self.panes() - 1);
    }

    fn reset_selection(&mut self) {
        for s in [&mut self.artists, &mut self.albums, &mut self.left] {
            s.select(Some(0));
        }
        self.songs.select(Some(0));
        self.right.select(Some(0));
    }

    fn event_loop(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        loop {
            terminal.draw(|f| self.draw(f))?;
            if !event::poll(Duration::from_millis(250))? {
                continue;
            }
            let Event::Key(key) = event::read()? else { continue };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if self.searching {
                match key.code {
                    KeyCode::Esc => {
                        self.query.clear();
                        self.searching = false;
                    }
                    KeyCode::Enter => self.searching = false,
                    KeyCode::Backspace => {
                        self.query.pop();
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => self.query.clear(),
                    KeyCode::Char(c) => self.query.push(c),
                    _ => {}
                }
                self.reset_selection();
                continue;
            }
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            let half = (terminal.size().map(|s| s.height).unwrap_or(40) as i64 / 2).max(1);
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char('c') if ctrl => return Ok(()),
                KeyCode::Char('/') => self.searching = true,
                KeyCode::Tab | KeyCode::Char('2') if self.tab == Tab::Library => self.set_tab(Tab::Playlists),
                KeyCode::Tab | KeyCode::BackTab | KeyCode::Char('1') if self.tab == Tab::Playlists => {
                    self.set_tab(Tab::Library)
                }
                KeyCode::BackTab => self.set_tab(Tab::Playlists),
                KeyCode::Left | KeyCode::Char('h') => self.pane = self.pane.saturating_sub(1),
                KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => {
                    self.pane = (self.pane + 1).min(self.panes() - 1)
                }
                KeyCode::Char('r') => {
                    let remote: Vec<_> = self
                        .entries
                        .iter()
                        .filter_map(|e| match e {
                            Entry::Remote { target, playlist } => Some((*target, playlist.clone())),
                            _ => None,
                        })
                        .collect();
                    self.reload(remote)?;
                    self.status = "reloaded".into();
                }
                KeyCode::Down | KeyCode::Char('j') => self.step(1)?,
                KeyCode::Up | KeyCode::Char('k') => self.step(-1)?,
                KeyCode::Char('d') if ctrl => self.step(half)?,
                KeyCode::Char('u') if ctrl => self.step(-half)?,
                KeyCode::PageDown => self.step(half * 2)?,
                KeyCode::PageUp => self.step(-half * 2)?,
                KeyCode::Char('f') if ctrl => self.step(half * 2)?,
                KeyCode::Char('b') if ctrl => self.step(-half * 2)?,
                KeyCode::Home | KeyCode::Char('g') => self.step(i64::MIN / 2)?,
                KeyCode::End | KeyCode::Char('G') => self.step(i64::MAX / 2)?,
                _ => {}
            }
        }
    }

    fn step(&mut self, by: i64) -> Result<()> {
        let (len, cur) = match (self.tab, self.pane) {
            (Tab::Library, 0) => (self.shelf().artists.len(), self.artists.selected()),
            (Tab::Library, 1) => (self.shelf().albums.len(), self.albums.selected()),
            (Tab::Library, _) => (self.shelf().songs.len(), self.songs.selected()),
            (Tab::Playlists, 0) => (self.entries.len(), self.left.selected()),
            (Tab::Playlists, _) => (self.visible().len(), self.right.selected()),
        };
        if len == 0 {
            return Ok(());
        }
        let next = (cur.unwrap_or(0).min(len - 1) as i64 + by).clamp(0, len as i64 - 1) as usize;
        match (self.tab, self.pane) {
            (Tab::Library, 0) => {
                self.artists.select(Some(next));
                self.albums.select(Some(0));
                self.songs.select(Some(0));
            }
            (Tab::Library, 1) => {
                self.albums.select(Some(next));
                self.songs.select(Some(0));
            }
            (Tab::Library, _) => self.songs.select(Some(next)),
            (Tab::Playlists, 0) => self.select_left(next)?,
            (Tab::Playlists, _) => self.right.select(Some(next)),
        }
        Ok(())
    }

    fn draw(&mut self, f: &mut Frame) {
        let [tabs, body, detail, help] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(5), Constraint::Length(3), Constraint::Length(1)])
                .areas(f.area());
        let filter = if self.query.is_empty() { String::new() } else { format!("  /{}", self.query) };
        f.render_widget(
            Tabs::new(TABS).select(self.tab as usize).highlight_style(Style::default().bold().cyan()).divider("│"),
            tabs,
        );
        f.render_widget(Paragraph::new(filter.yellow()).right_aligned(), tabs);
        match self.tab {
            Tab::Library => self.draw_library(f, body),
            Tab::Playlists => {
                let [left, right] =
                    Layout::horizontal([Constraint::Percentage(32), Constraint::Percentage(68)]).areas(body);
                self.draw_playlists(f, left);
                self.draw_playlist_tracks(f, right);
            }
        }
        self.draw_detail(f, detail);
        let keys = if self.searching {
            " type to filter · Enter keep · Esc clear · ^U wipe "
        } else {
            " Tab/1/2 tabs · hjkl move · ^D/^U half page · ^F/^B page · g/G ends · / filter · r reload · q quit "
        };
        f.render_widget(Paragraph::new(Line::from(vec![keys.dim(), self.status.clone().yellow()])), help);
    }

    fn draw_library(&mut self, f: &mut Frame, area: Rect) {
        let shelf =
            shelf(&self.all, &self.query, self.artists.selected().unwrap_or(0), self.albums.selected().unwrap_or(0));
        let [a, b, c] =
            Layout::horizontal([Constraint::Percentage(25), Constraint::Percentage(30), Constraint::Percentage(45)])
                .areas(area);
        let highlight = Style::default().add_modifier(Modifier::REVERSED);

        let items: Vec<ListItem> = shelf.artists.iter().map(|(n, c)| ListItem::new(format!("{n} ({c})"))).collect();
        let title = format!(" Artists ({}) ", shelf.artists.len());
        let list = List::new(items).block(pane_block(&title, self.pane == 0)).highlight_style(highlight);
        f.render_stateful_widget(list, a, &mut self.artists);

        let items: Vec<ListItem> = shelf
            .albums
            .iter()
            .map(|(name, year, n)| {
                let year = year.map(|y| format!(" · {y}")).unwrap_or_default();
                ListItem::new(format!("{name}{year} ({n})"))
            })
            .collect();
        let list = List::new(items).block(pane_block(" Albums ", self.pane == 1)).highlight_style(highlight);
        f.render_stateful_widget(list, b, &mut self.albums);

        let rows: Vec<Row> = shelf
            .songs
            .iter()
            .map(|t| {
                Row::new(vec![
                    Cell::from(t.track_no.map(|n| n.to_string()).unwrap_or_default()),
                    status_cell(&t.status),
                    Cell::from(t.title.clone()),
                    Cell::from(mmss(t.duration_ms)),
                ])
            })
            .collect();
        let table = Table::new(
            rows,
            [Constraint::Length(3), Constraint::Length(6), Constraint::Min(10), Constraint::Length(5)],
        )
        .header(Row::new(["#", "STATUS", "TITLE", "LEN"]).bold())
        .block(pane_block(" Songs ", self.pane == 2))
        .row_highlight_style(highlight);
        f.render_stateful_widget(table, c, &mut self.songs);
    }

    fn draw_playlists(&mut self, f: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .entries
            .iter()
            .map(|e| match e {
                Entry::Local(p) => {
                    let synced = if p.synced.is_empty() {
                        String::new()
                    } else {
                        format!(" → {}", p.synced.iter().map(|s| s.target.as_str()).collect::<Vec<_>>().join(","))
                    };
                    ListItem::new(format!("{} ({}){synced}", p.name, p.track_count))
                }
                Entry::Remote { target, playlist } => ListItem::new(
                    format!(
                        "{target}: {} ({}){}",
                        playlist.title,
                        playlist.tracks,
                        if playlist.smart { " ✦" } else { "" }
                    )
                    .dim(),
                ),
            })
            .collect();
        let list = List::new(items)
            .block(pane_block(" Playlists ", self.pane == 0))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        f.render_stateful_widget(list, area, &mut self.left);
    }

    fn draw_playlist_tracks(&mut self, f: &mut Frame, area: Rect) {
        if let Some(Entry::Remote { target, playlist }) = self.entries.get(self.left.selected().unwrap_or(0)) {
            let msg = format!("{} tracks on {target}; tapedeck only lists its own tracks here", playlist.tracks);
            f.render_widget(Paragraph::new(msg).block(pane_block(" Tracks ", false)), area);
            return;
        }
        let rows: Vec<Row> = self
            .visible()
            .into_iter()
            .map(|t| {
                Row::new(vec![
                    status_cell(&t.status),
                    Cell::from(t.artist.clone()),
                    Cell::from(t.title.clone()),
                    Cell::from(t.album.clone()),
                    Cell::from(t.year.map(|y| y.to_string()).unwrap_or_default()),
                    Cell::from(mmss(t.duration_ms)),
                ])
            })
            .collect();
        let table = Table::new(
            rows,
            [
                Constraint::Length(6),
                Constraint::Percentage(25),
                Constraint::Percentage(35),
                Constraint::Percentage(30),
                Constraint::Length(4),
                Constraint::Length(5),
            ],
        )
        .header(Row::new(["STATUS", "ARTIST", "TITLE", "ALBUM", "YEAR", "LEN"]).bold())
        .block(pane_block(" Tracks ", self.pane == 1))
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        f.render_stateful_widget(table, area, &mut self.right);
    }

    fn draw_detail(&self, f: &mut Frame, area: Rect) {
        let text = match self.selected_track() {
            Some(t) => {
                let mut parts = vec![format!("#{}", t.id), format!("{} - {}", t.artist, t.title)];
                if let Some(p) = &t.path {
                    parts.push(p.clone());
                }
                if let Some(u) = &t.source_url {
                    parts.push(u.clone());
                }
                if let Some(e) = &t.error {
                    parts.push(format!("error: {e}"));
                }
                parts.join("  ·  ")
            }
            None => String::new(),
        };
        f.render_widget(Paragraph::new(text).block(Block::bordered().title(" Selected ")), area);
    }
}

fn status_cell(status: &str) -> Cell<'static> {
    match status {
        "ready" => Cell::from("ready").green(),
        "failed" => Cell::from("failed").red(),
        s => Cell::from(s.to_string()).yellow(),
    }
}

fn pane_block(title: &str, focused: bool) -> Block<'_> {
    let b = Block::bordered().title(title.to_string());
    if focused { b.border_style(Style::default().cyan()) } else { b }
}

fn mmss(ms: Option<i64>) -> String {
    match ms {
        Some(ms) => format!("{}:{:02}", ms / 60000, (ms / 1000) % 60),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(album_artist: &str, album: &str, title: &str, no: i64) -> Track {
        Track {
            id: 0,
            recording_mbid: String::new(),
            title: title.into(),
            artist: album_artist.into(),
            album_artist: album_artist.into(),
            album: album.into(),
            year: Some(2020),
            track_no: Some(no),
            path: None,
            status: "ready".into(),
            source_url: None,
            error: None,
            artist_mbid: None,
            album_mbid: String::new(),
            album_artist_mbid: String::new(),
            duration_ms: None,
        }
    }

    #[test]
    fn shelf_groups_artist_then_album() {
        let all = vec![
            track("bicep", "Isles", "Apricots", 2),
            track("Bicep", "Isles", "Atlas", 1),
            track("Bicep", "Bicep", "Glue", 5),
            track("Bonobo", "Fragments", "Rosewood", 3),
        ];
        let s = shelf(&all, "", 0, 1);
        assert_eq!(s.artists, vec![("Bicep", 3), ("Bonobo", 1)]);
        assert_eq!(s.albums, vec![("Bicep", Some(2020), 1), ("Isles", Some(2020), 2)]);
        assert_eq!(s.songs.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(), ["Atlas", "Apricots"]);
        // Out-of-range selections clamp; a filter narrows every column.
        assert_eq!(shelf(&all, "", 9, 9).songs.len(), 1);
        assert_eq!(shelf(&all, "rose", 0, 0).artists, vec![("Bonobo", 1)]);
        assert!(shelf(&all, "zzz", 0, 0).songs.is_empty());
    }
}
