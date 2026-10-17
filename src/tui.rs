//! `td tui` (and plain `td`): browse the library as artist › album › song, and the playlists
//! next to their tracks. Browsing reads the database directly; actions run the same `Command`s
//! the CLI has, through the daemon when it is up, else in-process, and every destructive one
//! asks first.

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
use tokio::{runtime::Handle, sync::mpsc, task::JoinHandle};

use crate::{
    Event as TdEvent,
    cli::{Command, RemoteCmd},
    config::Config,
    daemon,
    db::{Db, Playlist, Track},
    render,
    sync::{self, RemotePlaylist},
};

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Library = 0,
    Playlists = 1,
}

const TABS: [&str; 2] = ["Library", "Playlists"];
const SPINNER: [char; 4] = ['|', '/', '-', '\\'];

/// What the playlists tab's left pane can point at.
enum Entry {
    Local(Playlist),
    Remote { target: &'static str, playlist: RemotePlaylist },
}

enum Mode {
    Normal,
    Search,
    /// Asking for a download URL for one track.
    Prompt {
        track: i64,
        input: String,
    },
    /// `y` does `then`, anything else cancels.
    Confirm {
        msg: String,
        then: Pending,
    },
}

enum Pending {
    Run(Vec<Command>),
    Quit,
}

struct Tui {
    config: Config,
    rt: Handle,
    db: Db,
    tab: Tab,
    /// Focused pane within the tab, left to right.
    pane: usize,
    mode: Mode,
    query: String,
    status: String,
    /// The running action, if any, and its event stream.
    job: Option<(JoinHandle<()>, mpsc::Receiver<TdEvent>)>,
    log: Vec<String>,
    ticks: usize,
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
        config,
        rt: Handle::current(),
        db,
        tab: Tab::Library,
        pane: 0,
        mode: Mode::Normal,
        query: String::new(),
        status: String::new(),
        job: None,
        log: vec![],
        ticks: 0,
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

fn sync_now(no_fetch: bool) -> Command {
    Command::Sync { playlist: None, no_push: false, no_fetch }
}

impl Tui {
    fn reload(&mut self, remote: Vec<(&'static str, RemotePlaylist)>) -> Result<()> {
        self.all = self.db.all_tracks()?;
        self.entries = self.db.playlists()?.into_iter().map(Entry::Local).collect();
        self.entries.extend(remote.into_iter().map(|(target, playlist)| Entry::Remote { target, playlist }));
        self.select_left(self.left.selected().unwrap_or(0).min(self.entries.len().saturating_sub(1)))
    }

    /// Reload from the database, keeping the remote lists already fetched.
    fn refresh(&mut self) -> Result<()> {
        let remote: Vec<_> = self
            .entries
            .iter()
            .filter_map(|e| match e {
                Entry::Remote { target, playlist } => Some((*target, playlist.clone())),
                _ => None,
            })
            .collect();
        self.reload(remote)
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

    fn selected_entry(&self) -> Option<&Entry> {
        (self.tab == Tab::Playlists).then(|| self.entries.get(self.left.selected()?))?
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

    /// Run commands one after another on the runtime; events land in the log as they arrive.
    fn start(&mut self, cmds: Vec<Command>) {
        if self.job.is_some() {
            self.status = "busy: wait for the running action".into();
            return;
        }
        let (tx, rx) = mpsc::channel(64);
        let cfg = self.config.clone();
        let handle = self.rt.spawn(async move {
            for cmd in cmds {
                if let Err(e) = daemon::dispatch(&cfg, cmd, tx.clone()).await {
                    let _ = tx.send(TdEvent::Error { msg: format!("{e:#}") }).await;
                    break;
                }
            }
        });
        self.job = Some((handle, rx));
        self.status.clear();
    }

    fn poll_job(&mut self) -> Result<()> {
        let Some((handle, rx)) = &mut self.job else { return Ok(()) };
        while let Ok(ev) = rx.try_recv() {
            match ev {
                TdEvent::Info { msg } => self.log.push(msg),
                TdEvent::Error { msg } => self.log.push(format!("error: {msg}")),
                TdEvent::Data { data } => self.log.extend(render::render(&data).lines().map(str::to_string)),
            }
        }
        if handle.is_finished() {
            self.job = None;
            self.log.push("done".into());
            self.refresh()?;
        }
        if self.log.len() > 500 {
            self.log.drain(..self.log.len() - 500);
        }
        Ok(())
    }

    fn event_loop(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        loop {
            self.poll_job()?;
            self.ticks += 1;
            terminal.draw(|f| self.draw(f))?;
            if !event::poll(Duration::from_millis(250))? {
                continue;
            }
            let Event::Key(key) = event::read()? else { continue };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match std::mem::replace(&mut self.mode, Mode::Normal) {
                Mode::Search => {
                    self.mode = Mode::Search;
                    match key.code {
                        KeyCode::Esc => {
                            self.query.clear();
                            self.mode = Mode::Normal;
                        }
                        KeyCode::Enter => self.mode = Mode::Normal,
                        KeyCode::Backspace => {
                            self.query.pop();
                        }
                        KeyCode::Char('u') if ctrl => self.query.clear(),
                        KeyCode::Char(c) => self.query.push(c),
                        _ => {}
                    }
                    self.reset_selection();
                }
                Mode::Prompt { track, mut input } => match key.code {
                    KeyCode::Esc => self.status = "cancelled".into(),
                    KeyCode::Enter if !input.trim().is_empty() => {
                        self.start(vec![Command::Source { track, url: input.trim().to_string() }, sync_now(true)]);
                    }
                    KeyCode::Enter => {}
                    code => {
                        match code {
                            KeyCode::Backspace => {
                                input.pop();
                            }
                            KeyCode::Char('u') if ctrl => input.clear(),
                            KeyCode::Char(c) => input.push(c),
                            _ => {}
                        }
                        self.mode = Mode::Prompt { track, input };
                    }
                },
                Mode::Confirm { then, .. } => match (key.code, then) {
                    (KeyCode::Char('y') | KeyCode::Char('Y'), Pending::Run(cmds)) => self.start(cmds),
                    (KeyCode::Char('y') | KeyCode::Char('Y'), Pending::Quit) => return Ok(()),
                    _ => self.status = "cancelled".into(),
                },
                Mode::Normal => {
                    if self.normal_key(key.code, ctrl, terminal)? {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Normal-mode key; `Ok(true)` means quit.
    fn normal_key(&mut self, code: KeyCode, ctrl: bool, terminal: &ratatui::DefaultTerminal) -> Result<bool> {
        let half = (terminal.size().map(|s| s.height).unwrap_or(40) as i64 / 2).max(1);
        match code {
            // Quitting kills an in-process action half way; the daemon's would finish on its own.
            KeyCode::Char('q') | KeyCode::Esc if self.job.is_some() => {
                self.mode =
                    Mode::Confirm { msg: "An action is still running; quit anyway?".into(), then: Pending::Quit };
            }
            KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
            KeyCode::Char('c') if ctrl => return Ok(true),
            KeyCode::Char('/') => self.mode = Mode::Search,
            KeyCode::Tab | KeyCode::Char('2') if self.tab == Tab::Library => self.set_tab(Tab::Playlists),
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Char('1') if self.tab == Tab::Playlists => {
                self.set_tab(Tab::Library)
            }
            KeyCode::BackTab => self.set_tab(Tab::Playlists),
            KeyCode::Left | KeyCode::Char('h') => self.pane = self.pane.saturating_sub(1),
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => self.pane = (self.pane + 1).min(self.panes() - 1),
            KeyCode::Char('r') => {
                self.refresh()?;
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
            // Actions. Everything that deletes or re-downloads goes through Confirm.
            KeyCode::Char('S') => self.start(vec![sync_now(false)]),
            KeyCode::Char('R') => {
                if let Some((id, name)) = self.track_ref() {
                    self.mode = Mode::Confirm {
                        msg: format!("Re-download {name}? Its current file is dropped first."),
                        then: Pending::Run(vec![Command::Retry { track: Some(id) }, sync_now(true)]),
                    };
                }
            }
            KeyCode::Char('s') => {
                if let Some((track, _)) = self.track_ref() {
                    self.mode = Mode::Prompt { track, input: String::new() };
                }
            }
            KeyCode::Char('p') => match self.selected_entry() {
                Some(Entry::Local(p)) => {
                    self.mode = Mode::Confirm {
                        msg: format!("Push {} to every sync target?", p.name),
                        then: Pending::Run(vec![Command::Push { playlist: p.id.to_string(), to: None }]),
                    }
                }
                _ => self.status = "select a local playlist to push".into(),
            },
            KeyCode::Char('d') => match (self.pane, self.selected_entry()) {
                (0, Some(Entry::Remote { target, playlist })) => {
                    self.mode = Mode::Confirm {
                        msg: format!("Delete {} on {target}? A backup is written first.", playlist.title),
                        then: Pending::Run(vec![Command::Remote {
                            action: RemoteCmd::Rm {
                                pattern: playlist.title.clone(),
                                to: Some(target.to_string()),
                                yes: true,
                                include_smart: true,
                                no_backup: false,
                                out: None,
                            },
                        }]),
                    }
                }
                (0, Some(Entry::Local(_))) => {
                    self.status = "local playlists mirror ListenBrainz; delete tracks or remote copies instead".into()
                }
                _ => {
                    if let Some((id, name)) = self.track_ref() {
                        self.mode = Mode::Confirm {
                            msg: format!(
                                "Delete {name} and its file? A later sync re-adds it if a playlist still lists it."
                            ),
                            then: Pending::Run(vec![Command::Rm { track: id }]),
                        };
                    }
                }
            },
            _ => {}
        }
        Ok(false)
    }

    fn track_ref(&mut self) -> Option<(i64, String)> {
        match self.selected_track() {
            Some(t) => Some((t.id, format!("{} - {}", t.artist, t.title))),
            None => {
                self.status = "no track selected".into();
                None
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
            Layout::vertical([Constraint::Length(1), Constraint::Min(5), Constraint::Length(5), Constraint::Length(1)])
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
        let line = match &self.mode {
            Mode::Search => Line::from(" type to filter · Enter keep · Esc clear · ^U wipe ".dim()),
            Mode::Prompt { input, .. } => Line::from(vec![" download URL: ".bold(), input.clone().into(), "▏".dim()]),
            Mode::Confirm { msg, .. } => Line::from(vec![format!(" {msg} ").yellow(), "[y/N]".bold()]),
            Mode::Normal => Line::from(vec![
                " Tab tabs · hjkl · ^D/^U ^F/^B g/G · / filter · r reload · ".dim(),
                "S".bold(),
                " sync · ".dim(),
                "R".bold(),
                " re-download · ".dim(),
                "s".bold(),
                " source · ".dim(),
                "p".bold(),
                " push · ".dim(),
                "d".bold(),
                " delete · q quit ".dim(),
                self.status.clone().yellow(),
            ]),
        };
        f.render_widget(Paragraph::new(line), help);
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

    /// Selected track on the first line, the last two log lines below it.
    fn draw_detail(&self, f: &mut Frame, area: Rect) {
        let selected = match self.selected_track() {
            Some(t) => {
                let mut parts = vec![format!("#{}", t.id), format!("{} - {}", t.artist, t.title)];
                parts.extend(t.path.clone());
                parts.extend(t.source_url.clone());
                parts.extend(t.error.as_ref().map(|e| format!("error: {e}")));
                parts.join("  ·  ")
            }
            None => String::new(),
        };
        let tail = self.log.len().saturating_sub(2);
        let mut lines = vec![Line::from(selected)];
        lines.extend(self.log[tail..].iter().map(|l| Line::from(l.clone().dim())));
        let title = match self.job {
            Some(_) => format!(" Selected · running {} ", SPINNER[self.ticks % SPINNER.len()]),
            None => " Selected ".to_string(),
        };
        f.render_widget(Paragraph::new(lines).block(Block::bordered().title(title)), area);
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
