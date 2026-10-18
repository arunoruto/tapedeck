//! Playback through mpv's JSON IPC. tapedeck never decodes audio itself: it starts `mpv
//! --no-video --idle` with a Unix socket, hands it file paths (local) or `/stream/<id>` URLs
//! (remote, with the bearer header set at launch) and asks it for position and state once per
//! TUI tick. The queue is ours, so the bar can show artist and title rather than a URL.

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

pub struct Player {
    child: Child,
    sock: BufReader<UnixStream>,
    sock_path: PathBuf,
    next_id: u64,
    /// What we told mpv to play, in order: (track id, "Artist – Title").
    pub queue: Vec<(i64, String)>,
    pub volume: i64,
}

#[derive(Default, Clone, Debug)]
pub struct Status {
    pub label: String,
    pub track_id: Option<i64>,
    pub pos: f64,
    pub duration: f64,
    pub paused: bool,
    /// Nothing loaded.
    pub idle: bool,
}

impl Player {
    /// Start mpv. `header` is sent with every HTTP request (the remote API's bearer token).
    pub fn start(bin: &str, header: Option<&str>) -> Result<Player> {
        let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let sock_path = dir.join(format!("tapedeck-mpv-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock_path);
        let mut cmd = Command::new(bin);
        cmd.args([
            "--no-video",
            "--idle=yes",
            "--really-quiet",
            "--no-terminal",
            "--prefetch-playlist=yes",
            "--volume=100",
        ])
        .arg(format!("--input-ipc-server={}", sock_path.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
        if let Some(h) = header {
            cmd.arg(format!("--http-header-fields={h}"));
        }
        let child = cmd.spawn().with_context(|| format!("starting {bin} (set player.mpv_bin or install mpv)"))?;
        // mpv creates the socket a moment after it starts.
        let deadline = Instant::now() + Duration::from_secs(5);
        let stream = loop {
            match UnixStream::connect(&sock_path) {
                Ok(s) => break s,
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => bail!("mpv did not open its IPC socket: {e}"),
            }
        };
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        Ok(Player { child, sock: BufReader::new(stream), sock_path, next_id: 1, queue: vec![], volume: 100 })
    }

    fn cmd(&mut self, args: Vec<Value>) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let line = json!({ "command": args, "request_id": id }).to_string();
        self.sock.get_mut().write_all(format!("{line}\n").as_bytes())?;
        // Events arrive on the same socket; skip them until our reply comes.
        let mut buf = String::new();
        loop {
            buf.clear();
            if self.sock.read_line(&mut buf)? == 0 {
                bail!("mpv closed the IPC socket");
            }
            let v: Value = match serde_json::from_str(&buf) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v["request_id"].as_u64() == Some(id) {
                if v["error"].as_str().is_some_and(|e| e != "success") {
                    bail!("mpv: {}", v["error"]);
                }
                return Ok(v["data"].clone());
            }
        }
    }

    /// Replace the queue with `items` and start on the first.
    pub fn play(&mut self, items: Vec<(i64, String, String)>) -> Result<()> {
        self.queue.clear();
        self.cmd(vec![json!("playlist-clear")])?;
        for (i, (id, label, url)) in items.into_iter().enumerate() {
            let how = if i == 0 { "replace" } else { "append" };
            self.cmd(vec![json!("loadfile"), json!(url), json!(how)])?;
            self.queue.push((id, label));
        }
        self.cmd(vec![json!("set_property"), json!("pause"), json!(false)])?;
        Ok(())
    }

    pub fn toggle_pause(&mut self) -> Result<()> {
        self.cmd(vec![json!("cycle"), json!("pause")]).map(drop)
    }

    pub fn play_next(&mut self) -> Result<()> {
        self.cmd(vec![json!("playlist-next"), json!("weak")]).map(drop)
    }

    pub fn play_prev(&mut self) -> Result<()> {
        self.cmd(vec![json!("playlist-prev"), json!("weak")]).map(drop)
    }

    pub fn seek(&mut self, secs: f64) -> Result<()> {
        self.cmd(vec![json!("seek"), json!(secs), json!("relative")]).map(drop)
    }

    pub fn volume(&mut self, delta: i64) -> Result<()> {
        self.volume = (self.volume + delta).clamp(0, 130);
        self.cmd(vec![json!("set_property"), json!("volume"), json!(self.volume)]).map(drop)
    }

    pub fn stop(&mut self) -> Result<()> {
        self.queue.clear();
        self.cmd(vec![json!("stop")]).map(drop)
    }

    pub fn status(&mut self) -> Status {
        let get = |p: &mut Player, name: &str| p.cmd(vec![json!("get_property"), json!(name)]).ok();
        let idle = get(self, "idle-active").and_then(|v| v.as_bool()).unwrap_or(true);
        if idle {
            return Status { idle: true, ..Default::default() };
        }
        let pos_in_queue = get(self, "playlist-pos").and_then(|v| v.as_i64()).unwrap_or(-1);
        let (track_id, label) = match usize::try_from(pos_in_queue).ok().and_then(|i| self.queue.get(i)) {
            Some((id, label)) => (Some(*id), label.clone()),
            None => (None, String::new()),
        };
        Status {
            label,
            track_id,
            pos: get(self, "time-pos").and_then(|v| v.as_f64()).unwrap_or(0.0),
            duration: get(self, "duration").and_then(|v| v.as_f64()).unwrap_or(0.0),
            paused: get(self, "pause").and_then(|v| v.as_bool()).unwrap_or(false),
            idle: false,
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.cmd(vec![json!("quit")]);
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.sock_path);
    }
}

pub fn mmss(secs: f64) -> String {
    let s = secs.max(0.0) as i64;
    format!("{}:{:02}", s / 60, s % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs mpv on PATH and a file in TAPEDECK_TEST_AUDIO; run with `--ignored --nocapture`.
    #[test]
    #[ignore = "needs mpv"]
    fn plays_a_file_and_reports_status() {
        let file = std::env::var("TAPEDECK_TEST_AUDIO").expect("TAPEDECK_TEST_AUDIO");
        let t0 = Instant::now();
        let mut p = Player::start("mpv", None).unwrap();
        eprintln!("started in {:?}", t0.elapsed());
        p.play(vec![(1, "A – B".into(), file)]).unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        let s = p.status();
        eprintln!("status after 1.5 s: {s:?}");
        assert!(!s.idle && s.pos > 0.5 && s.duration > 1.0, "{s:?}");
        p.toggle_pause().unwrap();
        assert!(p.status().paused);
        p.stop().unwrap();
        assert!(p.status().idle);
    }
}
