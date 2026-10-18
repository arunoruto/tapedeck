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
    style::Style,
    text::{Line, Span},
    widgets::{
        Block, BorderType, Cell, Clear, List, ListItem, ListState, Padding, Paragraph, Row, Table, TableState, Tabs,
    },
};
use tokio::{runtime::Handle, sync::mpsc, task::JoinHandle};

use crate::{
    Event as TdEvent,
    app::remote_title,
    cli::{Command, RemoteCmd},
    config::Config,
    daemon,
    db::{Db, Playlist, Track},
    render,
    sync::{self, RemotePlaylist},
    theme::Theme,
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
    /// A fetched playlist; `title` is its name on the targets, `on` the targets that have it now.
    Local {
        playlist: Playlist,
        title: String,
        on: Vec<&'static str>,
    },
    Remote {
        target: &'static str,
        playlist: RemotePlaylist,
    },
}

enum Mode {
    Normal,
    Search,
    /// Asking for one line of text about one track: a download URL, or a recording MBID.
    Prompt {
        track: i64,
        ask: Ask,
        input: String,
    },
    /// Everything known about the selected track, as key/value rows.
    Info(Vec<(&'static str, String)>),
    /// `y` does `then`, anything else cancels.
    Confirm {
        msg: String,
        then: Pending,
    },
}

#[derive(Clone, Copy, PartialEq)]
enum Ask {
    SourceUrl,
    Recording,
}

enum Pending {
    Run(Vec<Command>),
    Quit,
}

struct Tui {
    config: Config,
    theme: Theme,
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
    /// The last log line is a progress counter and gets replaced by the next one.
    progressing: bool,
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
    // Remote lists are fetched once up front; everything after that is local and instant. The
    // token may be readable only by the daemon's user; then the lists are simply absent and
    // actions still work, since they go through the daemon.
    let mut remote = vec![];
    let mut with_secrets = config.clone();
    match with_secrets.resolve_secrets() {
        Ok(()) => {
            for target in sync::from_config(&with_secrets)? {
                match target.list_playlists().await {
                    Ok(list) => remote.extend(list.into_iter().map(|p| (target.name(), p))),
                    Err(e) => tracing::warn!("{}: {e:#}", target.name()),
                }
            }
        }
        Err(e) => tracing::warn!("remote playlists not listed: {e:#}"),
    }
    let theme = Theme::load(&config.themes_dir(), &config.theme)?;
    let mut tui = Tui {
        config,
        theme,
        rt: Handle::current(),
        db,
        tab: Tab::Library,
        pane: 0,
        mode: Mode::Normal,
        query: String::new(),
        status: String::new(),
        job: None,
        log: vec![],
        progressing: false,
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
        let playlists = self.db.playlists()?;
        self.entries = playlists
            .iter()
            .map(|p| {
                let title = remote_title(&self.config, p, &playlists);
                let on = remote.iter().filter(|(_, r)| r.title == title).map(|(t, _)| *t).collect();
                Entry::Local { playlist: p.clone(), title, on }
            })
            .collect();
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
            Some(Entry::Local { playlist, .. }) => self.db.playlist_tracks(playlist.id)?,
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
                TdEvent::Info { msg } => {
                    self.progressing = false;
                    self.log.push(msg);
                }
                TdEvent::Progress { msg } => {
                    if self.progressing {
                        self.log.pop();
                    }
                    self.progressing = true;
                    self.log.push(msg);
                }
                TdEvent::Error { msg } => self.log.push(format!("error: {msg}")),
                TdEvent::Data { data } => self.log.extend(render::render(&data).lines().map(str::to_string)),
            }
        }
        if handle.is_finished() {
            self.job = None;
            self.progressing = false;
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
                Mode::Prompt { track, ask, mut input } => match key.code {
                    KeyCode::Esc => self.status = "cancelled".into(),
                    KeyCode::Enter if !input.trim().is_empty() => {
                        let text = input.trim().to_string();
                        self.mode = match ask {
                            Ask::SourceUrl => Mode::Confirm {
                                msg: format!("Download track #{track} from {text} and replace its file?"),
                                then: Pending::Run(vec![Command::Source { track, url: text }, sync_now(true)]),
                            },
                            Ask::Recording => Mode::Confirm {
                                msg: format!(
                                    "Make track #{track} recording {text}? It is retagged and moved if its path changes."
                                ),
                                then: Pending::Run(vec![Command::Retag { track, recording: text }]),
                            },
                        };
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
                        self.mode = Mode::Prompt { track, ask, input };
                    }
                },
                Mode::Confirm { then, .. } => match (key.code, then) {
                    (KeyCode::Char('y') | KeyCode::Char('Y'), Pending::Run(cmds)) => self.start(cmds),
                    (KeyCode::Char('y') | KeyCode::Char('Y'), Pending::Quit) => return Ok(()),
                    _ => self.status = "cancelled".into(),
                },
                Mode::Info(rows) => match key.code {
                    KeyCode::Esc | KeyCode::Enter | KeyCode::Char('i') | KeyCode::Char('q') => {}
                    _ => self.mode = Mode::Info(rows),
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
            KeyCode::Tab | KeyCode::Char(']') | KeyCode::Char('2') if self.tab == Tab::Library => {
                self.set_tab(Tab::Playlists)
            }
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Char('[') | KeyCode::Char('1') if self.tab == Tab::Playlists => {
                self.set_tab(Tab::Library)
            }
            KeyCode::BackTab | KeyCode::Char('[') => self.set_tab(Tab::Playlists),
            // Enter on the last pane (a track) opens its details.
            KeyCode::Enter | KeyCode::Char('i') if self.pane + 1 == self.panes() || code == KeyCode::Char('i') => {
                if let Some(rows) = self.selected_track().map(|t| track_info(&self.config, t)) {
                    self.mode = Mode::Info(rows);
                } else {
                    self.status = "no track selected".into();
                }
            }
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
            KeyCode::Char('S') => {
                self.mode = Mode::Confirm {
                    msg: "Run a full sync now (fetch, download, push)?".into(),
                    then: Pending::Run(vec![sync_now(false)]),
                }
            }
            KeyCode::Char('R') => {
                if let Some((id, name)) = self.track_ref() {
                    self.mode = Mode::Confirm {
                        msg: format!("Re-download {name} from a fresh search? Its current file is dropped first."),
                        then: Pending::Run(vec![Command::Retry { track: Some(id) }, sync_now(true)]),
                    };
                }
            }
            KeyCode::Char('s') => {
                if let Some((track, _)) = self.track_ref() {
                    self.mode = Mode::Prompt { track, ask: Ask::SourceUrl, input: String::new() };
                }
            }
            KeyCode::Char('m') => {
                if let Some((track, _)) = self.track_ref() {
                    self.mode = Mode::Prompt { track, ask: Ask::Recording, input: String::new() };
                }
            }
            KeyCode::Char('p') => match self.selected_entry() {
                Some(Entry::Local { playlist, title, .. }) => {
                    // The current playlist of a kind goes by id; an archived one by its file, so
                    // it lands on the target under its own (dated) title.
                    let arg = match &playlist.m3u_path {
                        Some(f) if remote_title(&self.config, playlist, std::slice::from_ref(playlist)) != *title => {
                            f.clone()
                        }
                        _ => playlist.id.to_string(),
                    };
                    self.mode = Mode::Confirm {
                        msg: format!(
                            "Push {title:?} to every sync target? A same-titled copy there is backed up and replaced."
                        ),
                        then: Pending::Run(vec![Command::Push { playlist: arg, to: None, title: None }]),
                    }
                }
                _ => self.status = "select a local playlist to push".into(),
            },
            KeyCode::Char('u') => match self.selected_entry() {
                Some(Entry::Local { title, on, .. }) if !on.is_empty() => {
                    self.mode = Mode::Confirm {
                        msg: format!("Remove {title:?} from {}? Backed up first; the local file stays.", on.join(", ")),
                        then: Pending::Run(vec![Command::Unpush { playlist: title.clone(), to: None, yes: true }]),
                    }
                }
                Some(Entry::Local { .. }) => self.status = "not on any target".into(),
                _ => self.status = "select a local playlist to remove from the targets".into(),
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
                (0, Some(Entry::Local { .. })) => {
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
        let th = self.theme.clone();
        // Theme text colour everywhere; the terminal's own background stays.
        f.render_widget(Block::new().style(Style::new().fg(th.text)), f.area());
        let [header, body, detail, help] =
            Layout::vertical([Constraint::Length(4), Constraint::Min(5), Constraint::Length(5), Constraint::Length(1)])
                .areas(f.area());
        self.draw_header(f, header);
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
        let key = |k: &str| Span::styled(k.to_string(), Style::new().fg(th.accent).bold());
        let dim = |t: &str| Span::styled(t.to_string(), Style::new().fg(th.muted));
        let line = match &self.mode {
            Mode::Search => Line::from(vec![
                dim(" type to filter · "),
                key("Enter"),
                dim(" keep · "),
                key("Esc"),
                dim(" clear · "),
                key("^U"),
                dim(" wipe "),
            ]),
            Mode::Prompt { input, ask, .. } => Line::from(vec![
                Span::styled(
                    match ask {
                        Ask::SourceUrl => " download URL: ",
                        Ask::Recording => " MusicBrainz recording id: ",
                    },
                    Style::new().fg(th.accent).bold(),
                ),
                input.clone().into(),
                Span::styled("▏", Style::new().fg(th.muted)),
            ]),
            Mode::Confirm { msg, .. } => {
                Line::from(vec![Span::styled(format!(" {msg} "), Style::new().fg(th.warning).bold()), key("[y/N]")])
            }
            Mode::Info(_) => {
                Line::from(vec![dim(" "), key("Esc"), dim("/"), key("Enter"), dim("/"), key("i"), dim(" close ")])
            }
            Mode::Normal => {
                let mut spans = vec![dim(" ")];
                for (i, (k, what)) in [
                    ("Tab/[]", "tabs"),
                    ("hjkl", "move"),
                    ("^D/^U", "half page"),
                    ("/", "filter"),
                    ("i", "info"),
                    ("S", "sync"),
                    ("R", "re-download"),
                    ("s", "source"),
                    ("m", "match"),
                    ("p", "push"),
                    ("u", "unpush"),
                    ("d", "delete"),
                    ("q", "quit"),
                ]
                .into_iter()
                .enumerate()
                {
                    if i > 0 {
                        spans.push(dim(" · "));
                    }
                    spans.push(key(k));
                    spans.push(dim(&format!(" {what}")));
                }
                spans.push(Span::styled(format!("  {}", self.status), Style::new().fg(th.warning)));
                Line::from(spans)
            }
        };
        f.render_widget(Paragraph::new(line), help);
        if let Mode::Info(rows) = &self.mode {
            self.draw_info(f, rows);
        }
    }

    /// A small cassette with the theme's stripes, the name, the tabs and the library's size.
    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let th = &self.theme;
        let [logo, right] = Layout::horizontal([Constraint::Length(13), Constraint::Min(10)]).areas(area);
        let frame = Style::new().fg(th.border);
        let mut stripes = vec![Span::styled("│ ", frame)];
        let blocks = 8;
        for i in 0..blocks {
            let colour = th.stripes[i * th.stripes.len() / blocks];
            stripes.push(Span::styled("▆", Style::new().fg(colour)));
        }
        stripes.push(Span::styled(" │", frame));
        let cassette = vec![
            Line::from(Span::styled("╭──────────╮", frame)),
            Line::from(stripes),
            Line::from(vec![
                Span::styled("│ ", frame),
                Span::styled("◉", Style::new().fg(th.accent)),
                Span::styled("──────", Style::new().fg(th.muted)),
                Span::styled("◉", Style::new().fg(th.accent)),
                Span::styled(" │", frame),
            ]),
            Line::from(Span::styled("╰──────────╯", frame)),
        ];
        f.render_widget(Paragraph::new(cassette), logo);

        // Tabs and counts sit level with the cassette's stripes and reels; no name, no theme.
        let [_, tabs, counts, filter] = Layout::vertical([Constraint::Length(1); 4]).areas(right);
        f.render_widget(
            Tabs::new(TABS)
                .select(self.tab as usize)
                .style(Style::new().fg(th.muted))
                .highlight_style(Style::new().fg(th.accent).bold())
                .divider(Span::styled("│", Style::new().fg(th.border))),
            tabs,
        );
        let artists = shelf(&self.all, "", 0, 0).artists.len();
        let locals = self.entries.iter().filter(|e| matches!(e, Entry::Local { .. })).count();
        let remotes = self.entries.len() - locals;
        let summary =
            format!("{artists} artists · {} tracks · {locals} playlists · {remotes} on targets", self.all.len());
        f.render_widget(Paragraph::new(Span::styled(summary, Style::new().fg(th.muted))), counts);
        if !self.query.is_empty() {
            f.render_widget(
                Paragraph::new(Span::styled(format!("filter: {}", self.query), Style::new().fg(th.warning))),
                filter,
            );
        }
    }

    /// Centered popup over everything else.
    fn draw_info(&self, f: &mut Frame, rows: &[(&str, String)]) {
        let th = &self.theme;
        let key_w = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
        let lines: Vec<Line> = rows
            .iter()
            .map(|(k, v)| {
                Line::from(vec![
                    Span::styled(format!("{k:>key_w$}  "), Style::new().fg(th.title).bold()),
                    v.clone().into(),
                ])
            })
            .collect();
        let area = f.area();
        let width = area.width.saturating_sub(4).clamp(20, 110);
        let height = (lines.len() as u16 + 2).min(area.height.saturating_sub(2));
        let popup = Rect {
            x: area.x + (area.width - width) / 2,
            y: area.y + (area.height.saturating_sub(height)) / 2,
            width,
            height,
        };
        f.render_widget(Clear, popup);
        f.render_widget(Paragraph::new(lines).block(self.pane(" Track ", true)), popup);
    }

    fn draw_library(&mut self, f: &mut Frame, area: Rect) {
        let th = self.theme.clone();
        let shelf =
            shelf(&self.all, &self.query, self.artists.selected().unwrap_or(0), self.albums.selected().unwrap_or(0));
        let [a, b, c] =
            Layout::horizontal([Constraint::Percentage(25), Constraint::Percentage(30), Constraint::Percentage(45)])
                .areas(area);
        let highlight = Style::new().bg(th.selection_bg).fg(th.selection_fg).bold();
        let count = |n: usize| Span::styled(format!(" {n}"), Style::new().fg(th.muted));

        let items: Vec<ListItem> = shelf
            .artists
            .iter()
            .map(|(n, c)| ListItem::new(Line::from(vec![n.to_string().into(), count(*c)])))
            .collect();
        let title = format!(" Artists ({}) ", shelf.artists.len());
        let list = List::new(items).block(self.pane(&title, self.pane == 0)).highlight_style(highlight);
        f.render_stateful_widget(list, a, &mut self.artists);

        let items: Vec<ListItem> = shelf
            .albums
            .iter()
            .map(|(name, year, n)| {
                let year = year.map(|y| format!(" · {y}")).unwrap_or_default();
                ListItem::new(Line::from(vec![
                    name.to_string().into(),
                    Span::styled(year, Style::new().fg(th.muted)),
                    count(*n),
                ]))
            })
            .collect();
        let list = List::new(items).block(self.pane(" Albums ", self.pane == 1)).highlight_style(highlight);
        f.render_stateful_widget(list, b, &mut self.albums);

        // The selected row is styled by hand: a table-wide highlight would paint over the badge.
        let selected = self.songs.selected();
        let rows: Vec<Row> = shelf
            .songs
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let sel = Some(i) == selected;
                let row = Row::new(vec![
                    Cell::from(Span::styled(
                        t.track_no.map(|n| n.to_string()).unwrap_or_default(),
                        Style::new().fg(th.muted),
                    )),
                    self.badge(&t.status, sel),
                    Cell::from(t.title.clone()),
                    Cell::from(Span::styled(mmss(t.duration_ms), Style::new().fg(th.muted))),
                ]);
                if sel { row.style(highlight) } else { row }
            })
            .collect();
        let table = Table::new(
            rows,
            [Constraint::Length(3), Constraint::Length(8), Constraint::Min(10), Constraint::Length(5)],
        )
        .header(self.header(["#", "STATUS", "TITLE", "LEN"]))
        .block(self.pane(" Songs ", self.pane == 2));
        f.render_stateful_widget(table, c, &mut self.songs);
    }

    fn draw_playlists(&mut self, f: &mut Frame, area: Rect) {
        let th = self.theme.clone();
        let items: Vec<ListItem> = self
            .entries
            .iter()
            .map(|e| match e {
                Entry::Local { playlist: p, title, on } => {
                    let mut spans = vec![Span::raw(title.clone())];
                    if *title != p.name {
                        spans.push(Span::styled(format!(": {}", p.name), Style::new().fg(th.muted)));
                    }
                    spans.push(Span::styled(format!(" {}", p.track_count), Style::new().fg(th.muted)));
                    if !on.is_empty() {
                        spans.push(Span::styled(format!("  ✓ {}", on.join(",")), Style::new().fg(th.ready)));
                    }
                    ListItem::new(Line::from(spans))
                }
                Entry::Remote { target, playlist } => ListItem::new(Line::from(Span::styled(
                    format!(
                        "{target}: {} {}{}",
                        playlist.title,
                        playlist.tracks,
                        if playlist.smart { " ✦" } else { "" }
                    ),
                    Style::new().fg(th.muted),
                ))),
            })
            .collect();
        let list = List::new(items)
            .block(self.pane(" Playlists ", self.pane == 0))
            .highlight_style(Style::new().bg(th.selection_bg).fg(th.selection_fg).bold());
        f.render_stateful_widget(list, area, &mut self.left);
    }

    fn draw_playlist_tracks(&mut self, f: &mut Frame, area: Rect) {
        let th = self.theme.clone();
        if let Some(Entry::Remote { target, playlist }) = self.entries.get(self.left.selected().unwrap_or(0)) {
            let msg = format!("{} tracks on {target}; tapedeck only lists its own tracks here", playlist.tracks);
            f.render_widget(
                Paragraph::new(Span::styled(msg, Style::new().fg(th.muted))).block(self.pane(" Tracks ", false)),
                area,
            );
            return;
        }
        let selected = self.right.selected();
        let highlight = Style::new().bg(th.selection_bg).fg(th.selection_fg).bold();
        let rows: Vec<Row> = self
            .visible()
            .into_iter()
            .enumerate()
            .map(|(i, t)| {
                let sel = Some(i) == selected;
                let row = Row::new(vec![
                    self.badge(&t.status, sel),
                    Cell::from(t.artist.clone()),
                    Cell::from(t.title.clone()),
                    Cell::from(Span::styled(t.album.clone(), Style::new().fg(th.muted))),
                    Cell::from(Span::styled(
                        t.year.map(|y| y.to_string()).unwrap_or_default(),
                        Style::new().fg(th.muted),
                    )),
                    Cell::from(Span::styled(mmss(t.duration_ms), Style::new().fg(th.muted))),
                ]);
                if sel { row.style(highlight) } else { row }
            })
            .collect();
        let table = Table::new(
            rows,
            [
                Constraint::Length(8),
                Constraint::Percentage(25),
                Constraint::Percentage(35),
                Constraint::Percentage(30),
                Constraint::Length(4),
                Constraint::Length(5),
            ],
        )
        .header(self.header(["STATUS", "ARTIST", "TITLE", "ALBUM", "YEAR", "LEN"]))
        .block(self.pane(" Tracks ", self.pane == 1));
        f.render_stateful_widget(table, area, &mut self.right);
    }

    /// Selected track on the first line, the last two log lines below it.
    fn draw_detail(&self, f: &mut Frame, area: Rect) {
        let th = &self.theme;
        let selected = match self.selected_track() {
            Some(t) => {
                let mut spans = vec![
                    Span::styled(format!("#{}  ", t.id), Style::new().fg(th.muted)),
                    Span::styled(format!("{} - {}", t.artist, t.title), Style::new().bold()),
                ];
                for part in [t.path.clone(), t.source_url.clone()].into_iter().flatten() {
                    spans.push(Span::styled(format!("  ·  {part}"), Style::new().fg(th.muted)));
                }
                if let Some(e) = &t.error {
                    spans.push(Span::styled(format!("  ·  {e}"), Style::new().fg(th.failed)));
                }
                Line::from(spans)
            }
            None => Line::default(),
        };
        let tail = self.log.len().saturating_sub(2);
        let mut lines = vec![selected];
        lines.extend(self.log[tail..].iter().map(|l| {
            let colour = if l.starts_with("error") { th.failed } else { th.muted };
            Line::from(Span::styled(l.clone(), Style::new().fg(colour)))
        }));
        let title = match self.job {
            Some(_) => format!(" Selected · running {} ", SPINNER[self.ticks % SPINNER.len()]),
            None => " Selected ".to_string(),
        };
        f.render_widget(Paragraph::new(lines).block(self.pane(&title, self.job.is_some())), area);
    }

    /// Rounded, padded pane; the focused one gets the accent border.
    fn pane(&self, title: &str, focused: bool) -> Block<'static> {
        let th = &self.theme;
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(if focused { th.accent } else { th.border }))
            .title(Span::styled(title.to_string(), Style::new().fg(if focused { th.accent } else { th.title }).bold()))
            .padding(Padding::horizontal(1))
    }

    fn header<const N: usize>(&self, cols: [&'static str; N]) -> Row<'static> {
        Row::new(cols.map(|c| Cell::from(Span::styled(c, Style::new().fg(self.theme.title).bold()))))
    }

    /// Status as a coloured pill; on the selected row it flips to the accent so it stands out
    /// from the other rows' pills as well as from the highlight.
    fn badge(&self, status: &str, selected: bool) -> Cell<'static> {
        let th = &self.theme;
        let colour = match status {
            "ready" => th.ready,
            "external" => th.title,
            "failed" => th.failed,
            _ => th.wanted,
        };
        let style = if selected {
            Style::new().bg(th.accent).fg(th.selection_bg).bold()
        } else {
            Style::new().bg(colour).fg(th.selection_bg).bold()
        };
        Cell::from(Span::styled(format!(" {status} "), style))
    }
}

/// Key/value rows for the info popup: tags, file properties, and the URLs where the track can
/// be heard or checked. Reads the file's header on the spot, so the quality shown is what is
/// on disk, not what the database remembers.
fn track_info(cfg: &Config, t: &Track) -> Vec<(&'static str, String)> {
    let mut rows = vec![
        ("title", t.title.clone()),
        ("artist", t.artist.clone()),
        (
            "album",
            match t.year {
                Some(y) => format!("{} ({y})", t.album),
                None => t.album.clone(),
            },
        ),
        ("album artist", t.album_artist.clone()),
        ("track", t.track_no.map(|n| n.to_string()).unwrap_or_default()),
        ("length", mmss(t.duration_ms)),
        ("status", t.status.clone()),
    ];
    if let Some(e) = &t.error {
        rows.push(("error", e.clone()));
    }
    if let Some(rel) = &t.path {
        let file = cfg.library.join(rel);
        rows.push(("file", file.display().to_string()));
        rows.push(("on disk", file_props(&file)));
    }
    if let Some(u) = &t.source_url {
        rows.push(("source", u.clone()));
    }
    rows.push(("play", format!("https://listenbrainz.org/player/?recording_mbids={}", t.recording_mbid)));
    rows.push(("recording", format!("https://musicbrainz.org/recording/{}", t.recording_mbid)));
    if !t.album_mbid.is_empty() {
        rows.push(("release", format!("https://musicbrainz.org/release/{}", t.album_mbid)));
    }
    let artist = t.artist_mbid.as_deref().filter(|m| !m.is_empty()).unwrap_or(&t.album_artist_mbid);
    if !artist.is_empty() {
        rows.push(("artist page", format!("https://musicbrainz.org/artist/{artist}")));
    }
    rows.push(("id", format!("#{}", t.id)));
    rows
}

/// "opus · 128 kbps · 48 kHz · 3:21 · 3.2 MiB", or why it could not be read.
fn file_props(path: &std::path::Path) -> String {
    use lofty::{config::ParseOptions, file::AudioFile, file::TaggedFileExt, probe::Probe};
    let size = match std::fs::metadata(path) {
        Ok(m) => format!("{:.1} MiB", m.len() as f64 / 1_048_576.0),
        Err(e) => return format!("missing ({e})"),
    };
    match Probe::open(path).and_then(|p| p.options(ParseOptions::new().read_tags(false)).read()) {
        Ok(f) => {
            let p = f.properties();
            let ms = p.duration().as_millis() as i64;
            let mut parts = vec![format!("{:?}", f.file_type()).to_lowercase()];
            parts.extend(p.audio_bitrate().map(|b| format!("{b} kbps")));
            parts.extend(p.sample_rate().map(|r| format!("{:.1} kHz", r as f64 / 1000.0)));
            parts.push(mmss(Some(ms)));
            parts.push(size);
            parts.join(" · ")
        }
        Err(e) => format!("{size}, unreadable: {e}"),
    }
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
