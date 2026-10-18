//! The daemon's HTTP face, for `td` on another machine: the same `Command`/`Event` protocol as
//! the Unix socket, over `POST /cmd` as NDJSON, plus `GET /stream/<track id>` serving a track's
//! file with Range support so a player can seek. Off unless `[server] listen` is set; every
//! request needs `Authorization: Bearer <token>`. The client side (`[remote] url`) lives here too.
//! The same listener also serves the Subsonic API under `/rest/` (src/subsonic.rs).

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Context, Result, bail};
use axum::{
    Router,
    body::Body,
    extract::{Path, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{any, get, post},
};
use tokio::sync::mpsc;
use tokio_stream::{StreamExt, wrappers::ReceiverStream};
use tower::ServiceExt;
use tower_http::services::ServeFile;

use crate::{App, Event, cli::Command, config::Config};

pub(crate) struct Shared {
    pub(crate) app: Arc<App>,
    /// The admin token, when one is configured.
    token: Option<String>,
    anonymous: Vec<String>,
}

/// Whether this request carries the admin token.
#[derive(Clone, Copy)]
pub(crate) struct Admin(pub(crate) bool);

/// What a command needs: "read", "stream", "listen", "request", "sync", or "admin" (everything else).
pub fn scope_of(cmd: &Command) -> &'static str {
    match cmd {
        Command::Status
        | Command::Search { .. }
        | Command::Playlists
        | Command::Tracks
        | Command::PlaylistTracks { .. }
        | Command::RemotePlaylists
        | Command::Artists
        | Command::Artist { folder: None, unpin: false, .. } => "read",
        Command::Listen { .. } => "listen",
        Command::Request { .. } => "request",
        Command::Sync { .. } => "sync",
        _ => "admin",
    }
}

pub async fn serve(app: Arc<App>, listen: &str, token: Option<String>, anonymous: Vec<String>) -> Result<()> {
    let addr: SocketAddr = listen.parse().with_context(|| format!("server.listen {listen:?} is not host:port"))?;
    if token.is_none() {
        tracing::info!("api: no [server] token; only the anonymous scopes {anonymous:?} are reachable over HTTP");
    }
    let shared = Arc::new(Shared { app, token, anonymous });
    let router = Router::new()
        .route("/cmd", post(cmd))
        .route("/stream/{id}", get(stream))
        .route("/ping", get(|| async { "tapedeck" }))
        .route("/rest/{method}", any(crate::subsonic::rest))
        .layer(middleware::from_fn_with_state(shared.clone(), auth))
        .with_state(shared);
    let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("binding {addr}"))?;
    tracing::info!("api listening on http://{addr}");
    axum::serve(listener, router).await?;
    Ok(())
}

/// A wrong token is refused outright; no token means the anonymous scopes.
async fn auth(State(s): State<Arc<Shared>>, mut req: Request, next: Next) -> Response {
    let sent = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string);
    let admin = match (&sent, &s.token) {
        (Some(t), Some(want)) if t == want => true,
        (Some(_), _) => return (StatusCode::UNAUTHORIZED, "bad bearer token").into_response(),
        (None, _) => false,
    };
    req.extensions_mut().insert(Admin(admin));
    next.run(req).await
}

pub(crate) fn allowed(s: &Shared, admin: bool, scope: &str) -> bool {
    admin || s.anonymous.iter().any(|a| a == scope)
}

fn forbidden(s: &Shared, what: &str) -> Response {
    let hint = if s.token.is_some() {
        "needs the API token; run it on the server or set remote.token_file"
    } else {
        "needs the API token, and this server has none configured; run it on the server"
    };
    (StatusCode::FORBIDDEN, format!("`{what}` {hint}")).into_response()
}

/// Body: one `Command` as JSON. Response: `Event`s as NDJSON, streamed as they happen.
async fn cmd(
    State(s): State<Arc<Shared>>,
    axum::Extension(Admin(admin)): axum::Extension<Admin>,
    body: String,
) -> Response {
    let cmd: Command = match serde_json::from_str(&body) {
        Ok(c) => c,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("bad command: {e}")).into_response(),
    };
    if matches!(cmd, Command::Daemon | Command::Tui) {
        return (StatusCode::BAD_REQUEST, "that command runs in the foreground").into_response();
    }
    if !allowed(&s, admin, scope_of(&cmd)) {
        let name =
            serde_json::to_value(&cmd).ok().and_then(|v| v["cmd"].as_str().map(str::to_string)).unwrap_or_default();
        return forbidden(&s, &name);
    }
    let (tx, rx) = mpsc::channel::<Event>(64);
    let app = s.app.clone();
    tokio::spawn(async move { app.run(cmd, tx).await });
    let lines = ReceiverStream::new(rx).map(|ev| {
        let mut line = serde_json::to_string(&ev).unwrap_or_default();
        line.push('\n');
        Ok::<_, std::convert::Infallible>(line)
    });
    ([(header::CONTENT_TYPE, "application/x-ndjson")], Body::from_stream(lines)).into_response()
}

/// The track's file, own or external, with Range support (tower-http's ServeFile does the
/// headers, the partial reads and the content type).
async fn stream(State(s): State<Arc<Shared>>, Path(id): Path<i64>, req: Request) -> Response {
    let admin = req.extensions().get::<Admin>().map(|a| a.0).unwrap_or(false);
    if !allowed(&s, admin, "stream") {
        return forbidden(&s, "stream");
    }
    let track = match s.app.db().track_by_id(id) {
        Ok(Some(t)) => t,
        _ => return (StatusCode::NOT_FOUND, "no such track").into_response(),
    };
    let Some(rel) = track.path else { return (StatusCode::NOT_FOUND, "track has no file").into_response() };
    let file = s.app.config.library.join(rel);
    match ServeFile::new(&file).oneshot(req).await {
        Ok(res) => res.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    }
}

/// Client side: run `cmd` on the daemon behind `[remote] url`, relaying its events into `tx`.
pub async fn forward_remote(cfg: &Config, cmd: Command, tx: mpsc::Sender<Event>) -> Result<()> {
    let (url, token) = remote(cfg)?;
    let http = reqwest::Client::builder().user_agent(crate::USER_AGENT).build()?;
    let mut req = http.post(format!("{url}/cmd")).body(serde_json::to_string(&cmd)?);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let res = req.send().await.with_context(|| format!("reaching {url}"))?;
    if !res.status().is_success() {
        bail!("{url}: {} {}", res.status(), res.text().await.unwrap_or_default());
    }
    let mut res = res;
    let mut buf = String::new();
    while let Some(chunk) = res.chunk().await? {
        buf.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(nl) = buf.find('\n') {
            let line = buf[..nl].to_string();
            buf.drain(..=nl);
            if line.trim().is_empty() {
                continue;
            }
            let ev: Event = serde_json::from_str(&line).context("bad event from remote")?;
            if tx.send(ev).await.is_err() {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// `(url without trailing slash, token if any)` when a remote is configured.
pub fn remote(cfg: &Config) -> Result<(String, Option<String>)> {
    let url = cfg.remote.url.clone().context("no [remote] url configured")?;
    Ok((url.trim_end_matches('/').to_string(), cfg.remote.token.clone()))
}

/// Where a player fetches a track from: the remote stream URL, or the local file.
pub fn stream_url(cfg: &Config, track_id: i64, local_path: Option<&str>) -> Option<(String, Option<String>)> {
    if let Ok((url, token)) = remote(cfg) {
        return Some((format!("{url}/stream/{track_id}"), token));
    }
    local_path.map(|p| (cfg.library.join(p).to_string_lossy().into_owned(), None))
}

#[allow(dead_code)]
fn _paths(_: PathBuf, _: HeaderMap) {}
