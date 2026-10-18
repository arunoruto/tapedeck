//! The daemon's HTTP face, for `td` on another machine: the same `Command`/`Event` protocol as
//! the Unix socket, over `POST /cmd` as NDJSON, plus `GET /stream/<track id>` serving a track's
//! file with Range support so a player can seek. Off unless `[server] listen` is set; every
//! request needs `Authorization: Bearer <token>`. The client side (`[remote] url`) lives here too.

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Context, Result, bail};
use axum::{
    Router,
    body::Body,
    extract::{Path, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use tokio::sync::mpsc;
use tokio_stream::{StreamExt, wrappers::ReceiverStream};
use tower::ServiceExt;
use tower_http::services::ServeFile;

use crate::{App, Event, cli::Command, config::Config};

struct Shared {
    app: Arc<App>,
    token: String,
}

pub async fn serve(app: Arc<App>, listen: &str, token: String) -> Result<()> {
    let addr: SocketAddr = listen.parse().with_context(|| format!("server.listen {listen:?} is not host:port"))?;
    let shared = Arc::new(Shared { app, token });
    let router = Router::new()
        .route("/cmd", post(cmd))
        .route("/stream/{id}", get(stream))
        .route("/ping", get(|| async { "tapedeck" }))
        .layer(middleware::from_fn_with_state(shared.clone(), auth))
        .with_state(shared);
    let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("binding {addr}"))?;
    tracing::info!("api listening on http://{addr}");
    axum::serve(listener, router).await?;
    Ok(())
}

async fn auth(State(s): State<Arc<Shared>>, req: Request, next: Next) -> Response {
    let ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| t == s.token);
    if ok { next.run(req).await } else { (StatusCode::UNAUTHORIZED, "bad or missing bearer token").into_response() }
}

/// Body: one `Command` as JSON. Response: `Event`s as NDJSON, streamed as they happen.
async fn cmd(State(s): State<Arc<Shared>>, body: String) -> Response {
    let cmd: Command = match serde_json::from_str(&body) {
        Ok(c) => c,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("bad command: {e}")).into_response(),
    };
    if matches!(cmd, Command::Daemon | Command::Tui) {
        return (StatusCode::BAD_REQUEST, "that command runs in the foreground").into_response();
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
    let res = http
        .post(format!("{url}/cmd"))
        .bearer_auth(token)
        .body(serde_json::to_string(&cmd)?)
        .send()
        .await
        .with_context(|| format!("reaching {url}"))?;
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

/// `(url without trailing slash, token)` when a remote is configured.
pub fn remote(cfg: &Config) -> Result<(String, String)> {
    let url = cfg.remote.url.clone().context("no [remote] url configured")?;
    let token = cfg.remote.token.clone().context("[remote] needs token or token_file")?;
    Ok((url.trim_end_matches('/').to_string(), token))
}

/// Where a player fetches a track from: the remote stream URL, or the local file.
pub fn stream_url(cfg: &Config, track_id: i64, local_path: Option<&str>) -> Option<(String, Option<String>)> {
    if let Ok((url, token)) = remote(cfg) {
        return Some((format!("{url}/stream/{track_id}"), Some(token)));
    }
    local_path.map(|p| (cfg.library.join(p).to_string_lossy().into_owned(), None))
}

#[allow(dead_code)]
fn _paths(_: PathBuf, _: HeaderMap) {}
