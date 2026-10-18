//! Song requests over Telegram. Long polling, one allowed set of users, no inbound port. A
//! message with a link or "Artist - Title" is analysed; when sure, the song is fetched and
//! filed and the reply carries Undo / Wrong buttons; when not, the reply asks with inline
//! buttons. A 👎 reaction, the 👎 button or a "/wrong" reply reopens the question.

use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};

use crate::{
    App, Event,
    request::{Analysis, Choice, Outcome},
};

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
}

pub async fn run(app: Arc<App>, token: String, allowed: Vec<i64>) -> Result<()> {
    let bot = Arc::new(Bot {
        http: Client::builder().user_agent(crate::USER_AGENT).timeout(Duration::from_secs(90)).build()?,
        api: format!("https://api.telegram.org/bot{token}"),
        allowed,
        app,
        pending: Mutex::new(HashMap::new()),
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
                let help = "Send me a link (YouTube, YouTube Music, …) or \"Artist - Title\". I find the recording, fetch the best audio and add it to this year's Requests playlist. React 👎 or reply /wrong if I picked the wrong song.";
                self.send(chat, help, None, None).await?;
                return Ok(());
            }
            if text.is_empty() {
                return Ok(());
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
            let thumbs_down =
                r["new_reaction"].as_array().into_iter().flatten().any(|e| e["emoji"].as_str() == Some("👎"));
            if thumbs_down && let (Some(chat), Some(mid)) = (r["chat"]["id"].as_i64(), r["message_id"].as_i64()) {
                return self.reopen(chat, mid).await;
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
                let buttons = json!([[{ "text": "👎 Wrong song", "callback_data": "w" }, { "text": "↩️ Undo", "callback_data": "u" }]]);
                let mid = self.send(chat, &text, reply_to, Some(buttons)).await?;
                // The callback data can't carry the message id before the message exists; the
                // buttons are rewritten with it now.
                self.rewire(chat, mid, json!([[{ "text": "👎 Wrong song", "callback_data": format!("w:{mid}") }, { "text": "↩️ Undo", "callback_data": format!("u:{mid}") }]])).await?;
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
