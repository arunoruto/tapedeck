//! Song requests over Telegram. Long polling, one allowed set of users, no inbound port. A
//! message with a link or "Artist - Title" is analysed; when sure, the song is fetched and
//! filed and the reply carries Undo / Wrong buttons; when not, the reply asks with inline
//! buttons. A 👎 reaction, the 👎 button or a "/wrong" reply reopens the question.
//!
//! A playlist link (`list=`) is `td import` in a chat: one message that reads the playlist,
//! then says how many entries are new, already here or unsure, with Import / Review / Cancel.
//! Review steps through the unsure ones (top candidates or Skip; never "this video"); Import
//! edits the same message with progress and ends with what failed. A song shared from inside a
//! playlist (`watch?v=…&list=…`) asks first whether the song or the playlist was meant.

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};

use crate::{
    App, Event,
    request::{Analysis, Choice, ImportReport, Outcome, PlaylistPlan, playlist_id},
};

/// A playlist read and waiting for Import, possibly mid-review.
struct PlaylistPending {
    plan: PlaylistPlan,
    /// (position, candidate index) chosen in review.
    picks: Vec<(usize, usize)>,
    /// Which unsure entry the review shows; `None` shows the summary.
    at: Option<usize>,
}

struct Pending {
    chat_id: i64,
    analysis: Analysis,
    /// The track filed for this request, when it was.
    track_id: Option<i64>,
}

pub struct Bot {
    http: Client,
    api: String,
    allowed: Vec<i64>,
    app: Arc<App>,
    /// Keyed by the id of the bot's own message that the buttons or reactions refer to.
    pending: Mutex<HashMap<i64, Pending>>,
    playlists: Mutex<HashMap<i64, PlaylistPending>>,
    /// "Song or playlist?" questions, keyed like `pending`, holding the link.
    ambiguous: Mutex<HashMap<i64, String>>,
}

pub async fn run(app: Arc<App>, token: String, allowed: Vec<i64>) -> Result<()> {
    let bot = Arc::new(Bot {
        http: Client::builder().user_agent(crate::USER_AGENT).timeout(Duration::from_secs(90)).build()?,
        api: format!("https://api.telegram.org/bot{token}"),
        allowed,
        app,
        pending: Mutex::new(HashMap::new()),
        playlists: Mutex::new(HashMap::new()),
        ambiguous: Mutex::new(HashMap::new()),
    });
    let me = bot.call("getMe", json!({})).await?;
    tracing::info!("telegram: requests via @{}", me["username"].as_str().unwrap_or("?"));
    let mut offset = 0i64;
    loop {
        let updates = match bot
            .call(
                "getUpdates",
                json!({ "offset": offset, "timeout": 50, "allowed_updates": ["message", "callback_query", "message_reaction"] }),
            )
            .await
        {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!("telegram: {e:#}");
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
        };
        for u in updates.as_array().into_iter().flatten() {
            offset = offset.max(u["update_id"].as_i64().unwrap_or(0) + 1);
            let bot = bot.clone();
            let u = u.clone();
            tokio::spawn(async move {
                if let Err(e) = bot.handle(u).await {
                    tracing::warn!("telegram: {e:#}");
                }
            });
        }
    }
}

impl Bot {
    async fn call(&self, method: &str, body: Value) -> Result<Value> {
        let v: Value = self
            .http
            .post(format!("{}/{method}", self.api))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("telegram {method}"))?
            .json()
            .await?;
        if v["ok"] != true {
            anyhow::bail!("telegram {method}: {}", v["description"].as_str().unwrap_or("error"));
        }
        Ok(v["result"].clone())
    }

    fn allowed(&self, user: &Value) -> bool {
        user["id"].as_i64().is_some_and(|id| self.allowed.contains(&id))
    }

    async fn handle(&self, u: Value) -> Result<()> {
        if let Some(m) = u.get("message").filter(|m| !m.is_null()) {
            if !self.allowed(&m["from"]) {
                return Ok(());
            }
            let chat = m["chat"]["id"].as_i64().context("no chat")?;
            let text = m["text"].as_str().unwrap_or("").trim().to_string();
            // "/wrong" as a reply to one of our messages reopens that request.
            if text.starts_with("/wrong")
                && let Some(reply) = m["reply_to_message"]["message_id"].as_i64()
            {
                return self.reopen(chat, reply).await;
            }
            if text.starts_with('/') {
                let help = "Send me a link (YouTube, YouTube Music, …) or \"Artist - Title\". I find the recording, fetch the best audio and add it to this year's Requests playlist. React 👍 (or tap Correct) when it's right, 👎 or reply /wrong when I picked the wrong song. A playlist link imports the playlist: I say what's new and what I'm unsure about before downloading anything.";
                self.send(chat, help, None, None).await?;
                return Ok(());
            }
            if text.is_empty() {
                return Ok(());
            }
            if text.contains("list=") {
                return self.playlist_link(chat, m["message_id"].as_i64(), &text).await;
            }
            return self.request(chat, m["message_id"].as_i64(), &text).await;
        }
        if let Some(q) = u.get("callback_query").filter(|q| !q.is_null()) {
            if !self.allowed(&q["from"]) {
                return Ok(());
            }
            let data = q["data"].as_str().unwrap_or("").to_string();
            let _ = self.call("answerCallbackQuery", json!({ "callback_query_id": q["id"] })).await;
            let chat = q["message"]["chat"]["id"].as_i64().context("no chat")?;
            return self.button(chat, &data).await;
        }
        if let Some(r) = u.get("message_reaction").filter(|r| !r.is_null()) {
            if !self.allowed(&r["user"]) {
                return Ok(());
            }
            let has = |emoji: &str| {
                r["new_reaction"].as_array().into_iter().flatten().any(|e| e["emoji"].as_str() == Some(emoji))
            };
            if let (Some(chat), Some(mid)) = (r["chat"]["id"].as_i64(), r["message_id"].as_i64()) {
                if has("👎") {
                    return self.reopen(chat, mid).await;
                }
                if has("👍") {
                    return self.confirm(chat, mid).await;
                }
            }
        }
        Ok(())
    }

    async fn request(&self, chat: i64, reply_to: Option<i64>, text: &str) -> Result<()> {
        let analysis = match self.app.analyze_request(text).await {
            Ok(a) => a,
            Err(e) => {
                self.send(chat, &format!("❌ {e:#}"), reply_to, None).await?;
                return Ok(());
            }
        };
        if analysis.sure {
            let outcome = self.fulfil(&analysis, Choice::Candidate(0)).await;
            self.report(chat, reply_to, analysis, outcome).await
        } else {
            let mid = self.ask(chat, reply_to, &analysis, "🤔").await?;
            self.pending.lock().await.insert(mid, Pending { chat_id: chat, analysis, track_id: None });
            Ok(())
        }
    }

    /// Run a choice under the daemon's lock, logging its events to the journal.
    async fn fulfil(&self, analysis: &Analysis, choice: Choice) -> Result<Outcome> {
        let _guard = self.app.busy.lock().await;
        let (tx, mut rx) = mpsc::channel::<Event>(64);
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                match ev {
                    Event::Info { msg } => tracing::info!("request: {msg}"),
                    Event::Error { msg } => tracing::error!("request: {msg}"),
                    _ => {}
                }
            }
        });
        self.app.fulfil_request(analysis, choice, &tx).await
    }

    /// "✅ Added …" (or ❌) with Undo / Wrong buttons; remembers the message for corrections.
    async fn report(
        &self,
        chat: i64,
        reply_to: Option<i64>,
        analysis: Analysis,
        outcome: Result<Outcome>,
    ) -> Result<()> {
        match outcome {
            Ok(o) => {
                let t = &o.track;
                let len =
                    t.duration_ms.map(|ms| format!(", {}:{:02}", ms / 60000, (ms / 1000) % 60)).unwrap_or_default();
                let what = format!("{} – {} ({}{len})", t.artist, t.title, t.album);
                let text = if o.already_had {
                    format!("✅ Already have it: {what}\nAdded to {}.", o.playlist)
                } else {
                    format!("✅ Added: {what}\nOn {}.", o.playlist)
                };
                let trio = |mid: &str| {
                    json!([[
                        { "text": "👍 Correct", "callback_data": format!("k:{mid}") },
                        { "text": "👎 Wrong song", "callback_data": format!("w:{mid}") },
                        { "text": "↩️ Undo", "callback_data": format!("u:{mid}") },
                    ]])
                };
                let mid = self.send(chat, &text, reply_to, Some(trio("0"))).await?;
                // The callback data can't carry the message id before the message exists; the
                // buttons are rewritten with it now.
                self.rewire(chat, mid, trio(&mid.to_string())).await?;
                self.pending.lock().await.insert(mid, Pending { chat_id: chat, analysis, track_id: Some(t.id) });
            }
            Err(e) => {
                self.send(chat, &format!("❌ {e:#}"), reply_to, None).await?;
            }
        }
        Ok(())
    }

    /// Ask which recording was meant. Returns the question's message id.
    async fn ask(&self, chat: i64, reply_to: Option<i64>, a: &Analysis, lead: &str) -> Result<i64> {
        let mut lines = vec![format!("{lead} {}: {}", a.source.raw_title, a.reason)];
        let mut rows: Vec<Value> = vec![];
        for (i, c) in a.candidates.iter().take(3).enumerate() {
            let len = c.length_ms.map(|ms| format!(", {}:{:02}", ms / 60000, (ms / 1000) % 60)).unwrap_or_default();
            lines.push(format!("{}. {} – {} ({}{len})", i + 1, c.artist, c.title, c.album));
            rows.push(json!([{ "text": format!("🎵 {}", i + 1), "callback_data": format!("c:MID:{i}") }]));
        }
        if a.source.url.is_some() {
            rows.push(json!([{ "text": "▶️ Exactly this video", "callback_data": "v:MID" }]));
        }
        rows.push(json!([{ "text": "✖ Cancel", "callback_data": "x:MID" }]));
        let mid = self.send(chat, &lines.join("\n"), reply_to, Some(json!(rows))).await?;
        let wired: Value = serde_json::from_str(&json!(rows).to_string().replace("MID", &mid.to_string()))?;
        self.rewire(chat, mid, wired).await?;
        Ok(mid)
    }

    async fn button(&self, chat: i64, data: &str) -> Result<()> {
        let mut parts = data.split(':');
        let (kind, mid) = (parts.next().unwrap_or(""), parts.next().and_then(|s| s.parse::<i64>().ok()));
        let Some(mid) = mid else { return Ok(()) };
        if kind.starts_with("pl") {
            return self.playlist_button(chat, kind, mid, parts.next()).await;
        }
        let pending = self.pending.lock().await.remove(&mid);
        let Some(p) = pending else {
            self.send(chat, "That request is no longer open (the daemon restarted?). Send the link again.", None, None)
                .await?;
            return Ok(());
        };
        match kind {
            "x" => {
                self.edit(chat, mid, "✖ Cancelled.").await?;
            }
            "u" => {
                if let Some(id) = p.track_id {
                    let _guard = self.app.busy.lock().await;
                    let (tx, mut rx) = mpsc::channel::<Event>(64);
                    tokio::spawn(async move { while rx.recv().await.is_some() {} });
                    match self.app.undo_request(id, &tx).await {
                        Ok(()) => self.edit(chat, mid, "↩️ Undone.").await?,
                        Err(e) => {
                            self.send(chat, &format!("❌ {e:#}"), Some(mid), None).await?;
                        }
                    }
                }
            }
            "w" => {
                self.pending.lock().await.insert(mid, p);
                self.reopen(chat, mid).await?;
            }
            "k" => {
                self.pending.lock().await.insert(mid, p);
                self.confirm(chat, mid).await?;
            }
            "c" | "v" => {
                let choice = match kind {
                    "v" => Choice::ThisVideo,
                    _ => Choice::Candidate(parts.next().and_then(|s| s.parse().ok()).unwrap_or(0)),
                };
                // Correcting an earlier answer: the earlier file goes first.
                if let Some(id) = p.track_id {
                    let _guard = self.app.busy.lock().await;
                    let (tx, mut rx) = mpsc::channel::<Event>(64);
                    tokio::spawn(async move { while rx.recv().await.is_some() {} });
                    let _ = self.app.undo_request(id, &tx).await;
                }
                self.edit(chat, mid, "⏳ Fetching…").await?;
                let outcome = self.fulfil(&p.analysis, choice).await;
                self.report(chat, None, p.analysis, outcome).await?;
            }
            _ => {}
        }
        Ok(())
    }

    /// 👍 / Correct: settle the request; the message loses its buttons so it can't be undone by
    /// a stray tap, and the row remembers the confirmation.
    async fn confirm(&self, chat: i64, mid: i64) -> Result<()> {
        let p = self.pending.lock().await.remove(&mid);
        let Some(p) = p else {
            self.send(chat, "Nothing open on that message (the daemon restarted?).", Some(mid), None).await?;
            return Ok(());
        };
        let Some(id) = p.track_id else { return Ok(()) };
        self.app.confirm_request(id)?;
        // Bind before the await: the DB guard must not live across it.
        let track = self.app.db().track_by_id(id)?;
        if let Some(t) = track {
            let len = t.duration_ms.map(|ms| format!(", {}:{:02}", ms / 60000, (ms / 1000) % 60)).unwrap_or_default();
            self.edit(chat, mid, &format!("✅ Confirmed: {} – {} ({}{len})", t.artist, t.title, t.album)).await?;
        }
        Ok(())
    }

    /// 👎 / Wrong: offer the choices again for a request we answered.
    async fn reopen(&self, chat: i64, mid: i64) -> Result<()> {
        let p = self.pending.lock().await.remove(&mid);
        let Some(p) = p else {
            self.send(chat, "I don't remember that one (the daemon restarted?). Send the link again.", Some(mid), None)
                .await?;
            return Ok(());
        };
        let qid = self.ask(chat, Some(mid), &p.analysis, "👎 Which one did you mean?").await?;
        self.pending
            .lock()
            .await
            .insert(qid, Pending { chat_id: p.chat_id, analysis: p.analysis, track_id: p.track_id });
        Ok(())
    }

    /// A link with `list=`: a playlist, a song shared from inside one (ask which), or a mix or
    /// private list (just the song if there is one).
    async fn playlist_link(&self, chat: i64, reply_to: Option<i64>, text: &str) -> Result<()> {
        let has_song = text.contains("v=");
        match playlist_id(text) {
            Err(_) if has_song => self.request(chat, reply_to, text).await,
            Err(e) => self.send(chat, &format!("❌ {e:#}"), reply_to, None).await.map(drop),
            Ok(id) if has_song => {
                // One flat read for the count; the matching waits until the playlist is wanted.
                let url = format!("https://www.youtube.com/playlist?list={id}");
                let n =
                    self.app.downloader.playlist(&url).await.ok().and_then(|v| v["entries"].as_array().map(Vec::len));
                let whole = n.map_or("the whole playlist".to_string(), |n| format!("the whole playlist ({n})"));
                let keys = |mid: &str| {
                    json!([
                        [{ "text": "🎵 This song", "callback_data": format!("pl1:{mid}") }],
                        [{ "text": format!("📃 {}", whole[..1].to_uppercase() + &whole[1..]), "callback_data": format!("plw:{mid}") }],
                        [{ "text": "✖ Cancel", "callback_data": format!("plx:{mid}") }],
                    ])
                };
                let mid = self.send(chat, &format!("Just this song, or {whole}?"), reply_to, Some(keys("0"))).await?;
                self.rewire(chat, mid, keys(&mid.to_string())).await?;
                self.ambiguous.lock().await.insert(mid, text.to_string());
                Ok(())
            }
            Ok(_) => self.playlist_analyze(chat, reply_to, None, text).await,
        }
    }

    /// Read and match the playlist in one message (`mid`, or a new one), then show the summary.
    async fn playlist_analyze(&self, chat: i64, reply_to: Option<i64>, mid: Option<i64>, url: &str) -> Result<()> {
        let mid = match mid {
            Some(m) => {
                self.edit(chat, m, "⏳ Reading the playlist…").await?;
                m
            }
            None => self.send(chat, "⏳ Reading the playlist…", reply_to, None).await?,
        };
        let (tx, rx) = mpsc::channel::<Event>(64);
        let app = &self.app;
        let analyze = async move { app.analyze_playlist(url, &tx).await };
        let (plan, ()) = tokio::join!(analyze, self.relay(chat, mid, rx, "⏳ Matching"));
        match plan {
            Ok(plan) => {
                let p = PlaylistPending { plan, picks: vec![], at: None };
                self.playlist_show(chat, mid, &p).await?;
                self.playlists.lock().await.insert(mid, p);
            }
            Err(e) => self.edit(chat, mid, &format!("❌ {e:#}")).await?,
        }
        Ok(())
    }

    /// Progress from a running job, as edits of one message at most every three seconds.
    async fn relay(&self, chat: i64, mid: i64, mut rx: mpsc::Receiver<Event>, lead: &str) {
        let mut last: Option<Instant> = None;
        while let Some(ev) = rx.recv().await {
            match ev {
                Event::Progress { msg, current: Some(c), total: Some(t) }
                    if last.is_none_or(|l| l.elapsed() >= Duration::from_secs(3)) =>
                {
                    last = Some(Instant::now());
                    let _ = self.edit(chat, mid, &format!("{lead} {c}/{t}: {msg}")).await;
                }
                Event::Info { msg } => tracing::info!("playlist: {msg}"),
                Event::Error { msg } => tracing::error!("playlist: {msg}"),
                _ => {}
            }
        }
    }

    async fn playlist_button(&self, chat: i64, kind: &str, mid: i64, arg: Option<&str>) -> Result<()> {
        let gone = "That playlist is no longer open (the daemon restarted?). Send the link again.";
        if matches!(kind, "pl1" | "plw") || (kind == "plx" && self.ambiguous.lock().await.contains_key(&mid)) {
            let link = self.ambiguous.lock().await.remove(&mid);
            let Some(link) = link else { return self.send(chat, gone, None, None).await.map(drop) };
            return match kind {
                "pl1" => {
                    self.edit(chat, mid, "🎵 Just this song.").await?;
                    self.request(chat, Some(mid), &link).await
                }
                "plw" => self.playlist_analyze(chat, None, Some(mid), &link).await,
                _ => self.edit(chat, mid, "✖ Cancelled.").await,
            };
        }
        let p = self.playlists.lock().await.remove(&mid);
        let Some(mut p) = p else { return self.send(chat, gone, None, None).await.map(drop) };
        let unsure = p.plan.unsure().count();
        match kind {
            "plx" => return self.edit(chat, mid, "✖ Cancelled.").await,
            "pli" => return self.playlist_import(chat, mid, p).await,
            "plr" => {
                p.picks.clear();
                p.at = Some(0);
            }
            "plc" | "pls" => {
                let at = p.at.unwrap_or(0);
                if kind == "plc"
                    && let (Some(e), Some(ci)) = (p.plan.unsure().nth(at), arg.and_then(|a| a.parse().ok()))
                {
                    let pos = e.pos;
                    p.picks.push((pos, ci));
                }
                p.at = Some(at + 1).filter(|n| *n < unsure);
            }
            _ => {}
        }
        self.playlist_show(chat, mid, &p).await?;
        self.playlists.lock().await.insert(mid, p);
        Ok(())
    }

    /// The summary with Import / Review / Cancel, or the unsure entry under review.
    async fn playlist_show(&self, chat: i64, mid: i64, p: &PlaylistPending) -> Result<()> {
        let plan = &p.plan;
        let unsure: Vec<_> = plan.unsure().collect();
        let take = plan.entries.iter().filter(|e| e.sure).count() + p.picks.len();
        let import = json!({ "text": format!("⬇️ Import {take}"), "callback_data": format!("pli:{mid}") });
        let (text, keys) = match p.at.and_then(|i| unsure.get(i).map(|e| (i, e))) {
            Some((i, e)) => {
                let mut lines =
                    vec![format!("🤔 {}/{}: {}", i + 1, unsure.len(), e.source.raw_title), e.reason.clone()];
                let mut row = vec![];
                for (ci, c) in e.candidates.iter().take(3).enumerate() {
                    let len =
                        c.length_ms.map(|ms| format!(", {}:{:02}", ms / 60000, (ms / 1000) % 60)).unwrap_or_default();
                    lines.push(format!("{}. {} – {} ({}{len})", ci + 1, c.artist, c.title, c.album));
                    row.push(json!({ "text": format!("🎵 {}", ci + 1), "callback_data": format!("plc:{mid}:{ci}") }));
                }
                let skip = json!({ "text": "⏭ Skip", "callback_data": format!("pls:{mid}") });
                (lines.join("\n"), json!([row, [skip, import]]))
            }
            None => {
                let new = plan.entries.iter().filter(|e| e.sure && !e.have).count();
                let have = plan.entries.iter().filter(|e| e.sure && e.have).count();
                let picked = if p.picks.is_empty() { String::new() } else { format!(", {} picked", p.picks.len()) };
                let text = format!(
                    "📃 {}: {} videos\n✅ {new} new, {have} already here, 🤔 {} unsure{picked}",
                    plan.name,
                    plan.entries.len(),
                    unsure.len()
                );
                let mut row = vec![import];
                if !unsure.is_empty() {
                    row.push(
                        json!({ "text": format!("🔍 Review {}", unsure.len()), "callback_data": format!("plr:{mid}") }),
                    );
                }
                row.push(json!({ "text": "✖ Cancel", "callback_data": format!("plx:{mid}") }));
                (text, json!([row]))
            }
        };
        self.call(
            "editMessageText",
            json!({ "chat_id": chat, "message_id": mid, "text": text, "reply_markup": { "inline_keyboard": keys } }),
        )
        .await
        .map(drop)
    }

    /// Import under the daemon's lock, with progress in the same message and the outcome last.
    async fn playlist_import(&self, chat: i64, mid: i64, p: PlaylistPending) -> Result<()> {
        self.edit(chat, mid, &format!("⏳ Importing {}…", p.plan.name)).await?;
        let _guard = self.app.busy.lock().await;
        let (tx, rx) = mpsc::channel::<Event>(64);
        let app = &self.app;
        let (plan, picks) = (&p.plan, &p.picks);
        let apply = async move { app.apply_playlist(plan, picks, &tx).await };
        let (report, ()) = tokio::join!(apply, self.relay(chat, mid, rx, "⏳ Importing"));
        let text = match report {
            Ok(r) => import_summary(&p.plan.name, &r, &self.app.targets.iter().map(|t| t.name()).collect::<Vec<_>>()),
            Err(e) => format!("❌ {e:#}"),
        };
        self.edit(chat, mid, &text).await
    }

    async fn send(&self, chat: i64, text: &str, reply_to: Option<i64>, keyboard: Option<Value>) -> Result<i64> {
        let mut body = json!({ "chat_id": chat, "text": text });
        if let Some(r) = reply_to {
            body["reply_parameters"] = json!({ "message_id": r, "allow_sending_without_reply": true });
        }
        if let Some(k) = keyboard {
            body["reply_markup"] = json!({ "inline_keyboard": k });
        }
        let m = self.call("sendMessage", body).await?;
        m["message_id"].as_i64().context("no message id")
    }

    async fn rewire(&self, chat: i64, mid: i64, keyboard: Value) -> Result<()> {
        self.call(
            "editMessageReplyMarkup",
            json!({ "chat_id": chat, "message_id": mid, "reply_markup": { "inline_keyboard": keyboard } }),
        )
        .await
        .map(drop)
    }

    async fn edit(&self, chat: i64, mid: i64, text: &str) -> Result<()> {
        self.call("editMessageText", json!({ "chat_id": chat, "message_id": mid, "text": text })).await.map(drop)
    }
}

/// "✅ Name: 10 tracks, 8 downloaded, on plex." plus the failures, kept under Telegram's limit.
fn import_summary(name: &str, r: &ImportReport, targets: &[&str]) -> String {
    let on = if targets.is_empty() {
        " The playlist file is written.".to_string()
    } else {
        format!(" On {}.", targets.join(", "))
    };
    let mut text = format!("✅ {name}: {} tracks, {} downloaded.{on}", r.tracks, r.downloaded);
    if !r.failed.is_empty() {
        text = format!("⚠️ {name}: {} tracks, {} downloaded, {} failed.{on}", r.tracks, r.downloaded, r.failed.len());
        for f in r.failed.iter().take(20) {
            text.push_str(&format!("\n• {f}"));
        }
        if r.failed.len() > 20 {
            text.push_str(&format!("\n… and {} more", r.failed.len() - 20));
        }
        text.push_str("\n`td retry` queues them again.");
    }
    text
}

#[cfg(test)]
mod tests {
    #[test]
    fn import_summary_lists_failures() {
        let r = super::ImportReport { tracks: 10, downloaded: 8, failed: vec!["A - x".into(), "B - y".into()] };
        let t = super::import_summary("Mix", &r, &["plex"]);
        assert!(t.starts_with("⚠️ Mix: 10 tracks, 8 downloaded, 2 failed. On plex."), "{t}");
        assert!(t.contains("\n• B - y"));
        let ok = super::import_summary("Mix", &super::ImportReport { tracks: 3, downloaded: 3, failed: vec![] }, &[]);
        assert_eq!(ok, "✅ Mix: 3 tracks, 3 downloaded. The playlist file is written.");
    }
}
