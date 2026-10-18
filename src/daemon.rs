//! Unix-socket server. Protocol: client writes one `Command` as a JSON line, the daemon
//! streams `Event` JSON lines back and closes. `td daemon` runs this; any other `td`
//! invocation connects if the socket is live and otherwise runs the command in-process.

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::mpsc,
};

use crate::{App, Event, cli::Command, config::Config};

pub async fn connect(cfg: &Config) -> Option<UnixStream> {
    UnixStream::connect(cfg.socket_path()).await.ok()
}

/// Client side: send the command, relay events into `tx`.
pub async fn forward(stream: UnixStream, cmd: Command, tx: mpsc::Sender<Event>) -> Result<()> {
    let (rd, mut wr) = stream.into_split();
    wr.write_all(format!("{}\n", serde_json::to_string(&cmd)?).as_bytes()).await?;
    wr.shutdown().await?;
    let mut lines = BufReader::new(rd).lines();
    while let Some(line) = lines.next_line().await? {
        let ev: Event = serde_json::from_str(&line).context("bad event from daemon")?;
        if tx.send(ev).await.is_err() {
            break;
        }
    }
    Ok(())
}

/// Run a command through the daemon when its socket is live, else in-process. Events go to `tx`;
/// failures inside the command arrive as `Event::Error`, the `Err` here is only for plumbing.
pub async fn dispatch(cfg: &Config, cmd: Command, tx: mpsc::Sender<Event>) -> Result<()> {
    match connect(cfg).await {
        Some(stream) => forward(stream, cmd, tx).await,
        None => {
            App::open(cfg.clone())?.run(cmd, tx).await;
            Ok(())
        }
    }
}

pub async fn serve(cfg: Config) -> Result<()> {
    let sock = cfg.socket_path();
    let interval = cfg.sync_interval()?;
    let app = Arc::new(App::open(cfg)?);

    if connect(&app.config).await.is_some() {
        anyhow::bail!("another daemon is already listening on {}", sock.display());
    }
    let _ = std::fs::remove_file(&sock); // stale from a previous run
    if let Some(dir) = sock.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let listener = UnixListener::bind(&sock).with_context(|| format!("binding {}", sock.display()))?;
    tracing::info!("listening on {}", sock.display());

    // Periodic sync. Events go to the log.
    let ticker = app.clone();
    let on_start = app.config.sync.on_start;
    tokio::spawn(async move {
        if !on_start {
            tracing::info!("sync.on_start is off; first sync in {interval:?}");
            tokio::time::sleep(interval).await;
        }
        loop {
            let (tx, mut rx) = mpsc::channel(64);
            let a = ticker.clone();
            tokio::spawn(
                async move { a.run(Command::Sync { playlist: None, no_push: false, no_fetch: false }, tx).await },
            );
            while let Some(ev) = rx.recv().await {
                log_event(&ev);
            }
            tokio::time::sleep(interval).await;
        }
    });

    // Periodic external-library scan, on its own clock, so a Lidarr change is noticed between syncs.
    if let Ok(every) = app.config.external_scan_interval()
        && !every.is_zero()
        && !app.config.external.paths.is_empty()
    {
        let scanner = app.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                let (tx, mut rx) = mpsc::channel(64);
                let a = scanner.clone();
                tokio::spawn(
                    async move { a.run(Command::External { action: crate::cli::ExternalCmd::Scan }, tx).await },
                );
                while let Some(ev) = rx.recv().await {
                    log_event(&ev);
                }
            }
        });
    }

    loop {
        let (stream, _) = listener.accept().await?;
        let app = app.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(app, stream).await {
                tracing::warn!("client: {e:#}");
            }
        });
    }
}

async fn handle(app: Arc<App>, stream: UnixStream) -> Result<()> {
    let (rd, mut wr) = stream.into_split();
    let mut line = String::new();
    BufReader::new(rd).read_line(&mut line).await?;
    let cmd: Command = serde_json::from_str(&line).context("bad command")?;
    let (tx, mut rx) = mpsc::channel(64);
    tokio::spawn(async move { app.run(cmd, tx).await });
    while let Some(ev) = rx.recv().await {
        // The client sees everything; the journal keeps the trace of what was done through the socket.
        log_event(&ev);
        wr.write_all(format!("{}\n", serde_json::to_string(&ev)?).as_bytes()).await?;
    }
    Ok(())
}

fn log_event(ev: &Event) {
    match ev {
        Event::Error { msg } => tracing::error!("{msg}"),
        Event::Info { msg } => tracing::info!("{msg}"),
        Event::Data { .. } | Event::Progress { .. } => {}
    }
}
