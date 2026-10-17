//! `td tui`: browse playlists and tracks in the terminal. Read-only for now: it opens the
//! database directly, so it works whether or not the daemon runs, and nothing here can break
//! the library.

use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style, Stylize},
    text::Line,
    widgets::{Block, Cell, List, ListItem, ListState, Paragraph, Row, Table, TableState},
};

use crate::{
    config::Config,
    db::{Db, Playlist, Track},
    sync::{self, RemotePlaylist},
};

/// What the left pane can point at.
enum Entry {
    Library,
    Local(Playlist),
    Remote { target: &'static str, playlist: RemotePlaylist },
}

#[derive(PartialEq, Clone, Copy)]
enum Focus {
    Left,
    Right,
    Search,
}

struct Tui {
    db: Db,
    entries: Vec<Entry>,
    left: ListState,
    tracks: Vec<Track>,
    right: TableState,
    query: String,
    focus: Focus,
    status: String,
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
        entries: vec![],
        left: ListState::default().with_selected(Some(0)),
        tracks: vec![],
        right: TableState::default().with_selected(Some(0)),
        query: String::new(),
        focus: Focus::Left,
        status: String::new(),
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

impl Tui {
    fn reload(&mut self, remote: Vec<(&'static str, RemotePlaylist)>) -> Result<()> {
        self.entries = vec![Entry::Library];
        self.entries.extend(self.db.playlists()?.into_iter().map(Entry::Local));
        self.entries.extend(remote.into_iter().map(|(target, playlist)| Entry::Remote { target, playlist }));
        self.select_left(self.left.selected().unwrap_or(0).min(self.entries.len().saturating_sub(1)))
    }

    fn select_left(&mut self, i: usize) -> Result<()> {
        self.left.select(Some(i));
        self.tracks = match &self.entries[i] {
            Entry::Library => self.db.all_tracks()?,
            Entry::Local(p) => self.db.playlist_tracks(p.id)?,
            Entry::Remote { .. } => vec![],
        };
        self.right.select(Some(0));
        Ok(())
    }

    fn visible(&self) -> Vec<&Track> {
        let q = self.query.to_lowercase();
        self.tracks
            .iter()
            .filter(|t| {
                q.is_empty()
                    || t.artist.to_lowercase().contains(&q)
                    || t.title.to_lowercase().contains(&q)
                    || t.album.to_lowercase().contains(&q)
            })
            .collect()
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
            if self.focus == Focus::Search {
                match key.code {
                    KeyCode::Esc => {
                        self.query.clear();
                        self.focus = Focus::Right;
                    }
                    KeyCode::Enter => self.focus = Focus::Right,
                    KeyCode::Backspace => {
                        self.query.pop();
                    }
                    KeyCode::Char(c) => self.query.push(c),
                    _ => {}
                }
                self.right.select(Some(0));
                continue;
            }
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
                KeyCode::Char('/') => self.focus = Focus::Search,
                KeyCode::Tab | KeyCode::Left | KeyCode::Right | KeyCode::Char('h') | KeyCode::Char('l') => {
                    self.focus = if self.focus == Focus::Left { Focus::Right } else { Focus::Left }
                }
                KeyCode::Char('r') => {
                    let remote = self.entries.iter().filter_map(|e| match e {
                        Entry::Remote { target, playlist } => Some((*target, playlist.clone())),
                        _ => None,
                    });
                    let remote: Vec<_> = remote.collect();
                    self.reload(remote)?;
                    self.status = "reloaded".into();
                }
                KeyCode::Down | KeyCode::Char('j') => self.step(1)?,
                KeyCode::Up | KeyCode::Char('k') => self.step(-1)?,
                KeyCode::PageDown => self.step(20)?,
                KeyCode::PageUp => self.step(-20)?,
                KeyCode::Home | KeyCode::Char('g') => self.step(i64::MIN / 2)?,
                KeyCode::End | KeyCode::Char('G') => self.step(i64::MAX / 2)?,
                KeyCode::Enter => self.focus = Focus::Right,
                _ => {}
            }
        }
    }

    fn step(&mut self, by: i64) -> Result<()> {
        let (len, cur) = match self.focus {
            Focus::Left => (self.entries.len(), self.left.selected().unwrap_or(0)),
            _ => (self.visible().len(), self.right.selected().unwrap_or(0)),
        };
        if len == 0 {
            return Ok(());
        }
        let next = (cur as i64 + by).clamp(0, len as i64 - 1) as usize;
        if self.focus == Focus::Left {
            self.select_left(next)?;
        } else {
            self.right.select(Some(next));
        }
        Ok(())
    }

    fn draw(&mut self, f: &mut Frame) {
        let [body, detail, help] =
            Layout::vertical([Constraint::Min(5), Constraint::Length(3), Constraint::Length(1)]).areas(f.area());
        let [left, right] = Layout::horizontal([Constraint::Percentage(32), Constraint::Percentage(68)]).areas(body);
        self.draw_left(f, left);
        self.draw_right(f, right);
        self.draw_detail(f, detail);
        let keys = match self.focus {
            Focus::Search => " type to filter · Enter keep · Esc clear ",
            _ => " ↑↓/jk move · Tab switch pane · / search · r reload · q quit ",
        };
        f.render_widget(Paragraph::new(Line::from(vec![keys.dim(), self.status.clone().yellow()])), help);
    }

    fn draw_left(&mut self, f: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .entries
            .iter()
            .map(|e| match e {
                Entry::Library => ListItem::new("Library (all tracks)".bold()),
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
            .block(pane_block(" Playlists ", self.focus == Focus::Left))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        f.render_stateful_widget(list, area, &mut self.left);
    }

    fn draw_right(&mut self, f: &mut Frame, area: Rect) {
        let title = match self.entries.get(self.left.selected().unwrap_or(0)) {
            Some(Entry::Remote { target, playlist }) => {
                let msg = format!("{} tracks on {target}; tapedeck only lists its own tracks here", playlist.tracks);
                f.render_widget(Paragraph::new(msg).block(pane_block(" Tracks ", false)), area);
                return;
            }
            _ if self.query.is_empty() => " Tracks ".to_string(),
            _ => format!(" Tracks /{} ", self.query),
        };
        let rows: Vec<Row> = self
            .visible()
            .into_iter()
            .map(|t| {
                let status = match t.status.as_str() {
                    "ready" => Cell::from("ready").green(),
                    "failed" => Cell::from("failed").red(),
                    s => Cell::from(s.to_string()).yellow(),
                };
                Row::new(vec![
                    status,
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
        .block(pane_block(&title, self.focus != Focus::Left))
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        f.render_stateful_widget(table, area, &mut self.right);
    }

    fn draw_detail(&self, f: &mut Frame, area: Rect) {
        let visible = self.visible();
        let text = match visible.get(self.right.selected().unwrap_or(0)) {
            Some(t) if !matches!(self.entries.get(self.left.selected().unwrap_or(0)), Some(Entry::Remote { .. })) => {
                let mut parts = vec![format!("#{}", t.id)];
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
            _ => String::new(),
        };
        f.render_widget(Paragraph::new(text).block(Block::bordered().title(" Selected ")), area);
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
