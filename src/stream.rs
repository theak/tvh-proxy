//! One TV, one remote.
//!
//! There is one tuner, so there is at most one *stream*: one ffmpeg process turning
//! the current channel into a 1-second-segment HLS playlist that every signed-in tab
//! watches. One tab holds the *remote* and can change the channel; everyone else
//! watches with a lock banner.
//!
//! Who holds the remote:
//! - The priority account, whenever one of its tabs is watching — it gets the remote
//!   the moment it shows up (no retune: the channel only changes when it changes it).
//! - Otherwise whoever started watching first; when they leave or press Stop the
//!   remote passes on (priority first, then longest-waiting).
//!
//! A stream nobody watches is stopped after a short grace period, so a page reload
//! doesn't retune the tuner. `Hub` is the pure bookkeeping (unit-tested); it returns
//! `Action`s that `Runner` carries out by starting/stopping ffmpeg.

use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncBufReadExt;

/// A browser tab that stops sending heartbeats for this long is considered gone.
pub const CLIENT_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a stream keeps running with nobody watching before ffmpeg is stopped.
pub const IDLE_GRACE: Duration = Duration::from_secs(20);

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "state", content = "message", rename_all = "lowercase")]
pub enum Status {
    Starting,
    Live,
    Error(String),
}

#[derive(Clone, Debug)]
struct Stream {
    /// New for each stream (each time playback starts from nothing).
    id: u64,
    channel: String,
    /// Bumped every time the stream is (re)tuned; part of the playlist URL so a
    /// browser never mixes segments from two different tunings.
    generation: u64,
    /// The tab holding the remote.
    owner: String,
    status: Status,
    empty_since: Option<Instant>,
}

#[derive(Clone, Debug)]
struct Tab {
    last_seen: Instant,
    joined: Instant,
    /// Pressed Stop: stay on the guide (don't auto-join the stream) until the tab
    /// picks a channel again.
    parked: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// (Re)start stream `id` at `generation` on `channel` (stopping any previous ffmpeg).
    Start {
        id: u64,
        generation: u64,
        channel: String,
    },
    Stop {
        id: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Not watching (nothing playing, or pressed Stop).
    Idle,
    /// Holds the remote.
    Controller,
    /// Watching; someone else holds the remote.
    Follower,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StreamView {
    pub id: u64,
    pub generation: u64,
    pub channel: String,
    pub status: Status,
    pub playlist: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct View {
    pub role: Role,
    pub stream: Option<StreamView>,
    /// Other tabs watching.
    pub others: usize,
}

#[derive(Debug, PartialEq)]
pub enum WatchError {
    /// Someone else holds the remote.
    Locked,
}

#[derive(Default)]
pub struct Hub {
    stream: Option<Stream>,
    tabs: HashMap<String, Tab>,
    /// Tabs signed in as the priority account.
    priority: HashSet<String>,
    next_id: u64,
}

impl Hub {
    pub fn new() -> Self {
        Hub {
            next_id: 1,
            ..Default::default()
        }
    }

    /// Mark whether `client` belongs to the priority account. Called on every
    /// request, so changing an account's priority takes effect immediately.
    pub fn mark_priority(&mut self, client: &str, priority: bool) {
        if priority {
            self.priority.insert(client.to_string());
        } else {
            self.priority.remove(client);
        }
    }

    /// Tabs watching the stream, in remote-passing order: priority first, then
    /// longest-waiting.
    fn watchers(&self) -> Vec<String> {
        if self.stream.is_none() {
            return vec![];
        }
        let mut w: Vec<(&String, &Tab)> = self.tabs.iter().filter(|(_, t)| !t.parked).collect();
        w.sort_by_key(|(name, t)| (!self.priority.contains(*name), t.joined));
        w.into_iter().map(|(n, _)| n.clone()).collect()
    }

    /// How many tabs are watching the stream right now (0 when nothing is playing).
    pub fn active_clients(&self) -> usize {
        self.watchers().len()
    }

    /// Re-decide who holds the remote: a watching priority tab always does;
    /// otherwise the current holder keeps it while watching, else it passes on.
    fn settle_remote(&mut self, now: Instant) {
        let watchers = self.watchers();
        let Some(s) = self.stream.as_mut() else {
            return;
        };
        if watchers.is_empty() {
            s.empty_since.get_or_insert(now);
            return;
        }
        s.empty_since = None;
        let owner_watching = watchers.contains(&s.owner);
        let owner_priority = self.priority.contains(&s.owner);
        let first = &watchers[0];
        if !owner_watching || (self.priority.contains(first) && !owner_priority) {
            s.owner = first.clone();
        }
    }

    /// Record that `client` is alive (every few seconds from each tab).
    pub fn heartbeat(&mut self, client: &str, now: Instant) {
        let tab = self.tabs.entry(client.to_string()).or_insert(Tab {
            last_seen: now,
            joined: now,
            parked: false,
        });
        tab.last_seen = now;
        self.settle_remote(now);
    }

    /// `client` asks to watch `channel`. `retry` restarts the same channel after an error.
    pub fn watch(
        &mut self,
        client: &str,
        channel: &str,
        retry: bool,
        now: Instant,
    ) -> Result<Vec<Action>, WatchError> {
        if let Some(t) = self.tabs.get_mut(client) {
            t.parked = false;
        }
        self.heartbeat(client, now);
        let Some(s) = self.stream.as_mut() else {
            let id = self.next_id;
            self.next_id += 1;
            self.stream = Some(Stream {
                id,
                channel: channel.to_string(),
                generation: 1,
                owner: client.to_string(),
                status: Status::Starting,
                empty_since: None,
            });
            return Ok(vec![Action::Start {
                id,
                generation: 1,
                channel: channel.to_string(),
            }]);
        };
        if s.owner != client {
            return Err(WatchError::Locked);
        }
        // Same channel = nothing to do (unless retrying a failed start). Otherwise one
        // restart; the Runner kills the old ffmpeg before starting the new one, so the
        // tuner is never asked for two channels at once.
        let failed = matches!(s.status, Status::Error(_));
        if s.channel == channel && !(retry && failed) {
            return Ok(vec![]);
        }
        s.channel = channel.to_string();
        s.generation += 1;
        s.status = Status::Starting;
        Ok(vec![Action::Start {
            id: s.id,
            generation: s.generation,
            channel: channel.to_string(),
        }])
    }

    /// The Stop button: this tab stops watching and stays on the guide. If nobody
    /// else is watching, the stream (and the tuner) is released immediately rather
    /// than after the idle grace period; otherwise the remote passes on.
    pub fn stop(&mut self, client: &str, now: Instant) -> Vec<Action> {
        self.heartbeat(client, now);
        self.tabs
            .get_mut(client)
            .expect("heartbeat inserted it")
            .parked = true;
        if self.stream.is_some() && self.watchers().is_empty() {
            let id = self.stream.take().unwrap().id;
            return vec![Action::Stop { id }];
        }
        self.settle_remote(now);
        vec![]
    }

    /// A tab closed (best-effort beacon from the browser).
    pub fn leave(&mut self, client: &str, now: Instant) -> Vec<Action> {
        self.tabs.remove(client);
        self.priority.remove(client);
        self.tick(now)
    }

    /// Housekeeping: drop silent tabs, pass the remote on, stop an unwatched stream.
    pub fn tick(&mut self, now: Instant) -> Vec<Action> {
        self.tabs
            .retain(|_, t| now.duration_since(t.last_seen) < CLIENT_TIMEOUT);
        let tabs = &self.tabs;
        self.priority.retain(|c| tabs.contains_key(c));
        self.settle_remote(now);
        if let Some(since) = self.stream.as_ref().and_then(|s| s.empty_since)
            && now.duration_since(since) >= IDLE_GRACE
        {
            let id = self.stream.take().unwrap().id;
            return vec![Action::Stop { id }];
        }
        vec![]
    }

    pub fn set_status(&mut self, id: u64, generation: u64, status: Status) {
        if let Some(s) = self.stream.as_mut()
            && s.id == id
            && s.generation == generation
        {
            s.status = status;
        }
    }

    pub fn is_current(&self, id: u64, generation: u64) -> bool {
        self.stream
            .as_ref()
            .is_some_and(|s| s.id == id && s.generation == generation)
    }

    pub fn view(&self, client: &str) -> View {
        let watching = self.tabs.get(client).is_some_and(|t| !t.parked);
        let Some(s) = self.stream.as_ref().filter(|_| watching) else {
            return View {
                role: Role::Idle,
                stream: None,
                others: 0,
            };
        };
        View {
            role: if s.owner == client {
                Role::Controller
            } else {
                Role::Follower
            },
            stream: Some(StreamView {
                id: s.id,
                generation: s.generation,
                channel: s.channel.clone(),
                status: s.status.clone(),
                playlist: format!("/hls/{}/{}/live.m3u8", s.id, s.generation),
            }),
            others: self.watchers().len().saturating_sub(1),
        }
    }

    fn view_status(&self, id: u64, generation: u64) -> Option<Status> {
        self.stream
            .as_ref()
            .filter(|s| s.id == id && s.generation == generation)
            .map(|s| s.status.clone())
    }

    pub fn view_status_pub(&self, id: u64) -> Option<Status> {
        self.stream
            .as_ref()
            .filter(|s| s.id == id)
            .map(|s| s.status.clone())
    }
}

/// How ffmpeg encodes. Defaults suit a modest x86 box; VAAPI offloads to an Intel iGPU.
#[derive(Clone, Debug)]
pub struct EncoderConfig {
    pub ffmpeg: String,
    pub hwaccel_vaapi: bool,
    pub vaapi_device: String,
    pub height: u32,
    pub video_bitrate_k: u32,
}

impl EncoderConfig {
    pub fn from_env() -> Self {
        fn var(k: &str) -> Option<String> {
            std::env::var(k).ok().filter(|s| !s.trim().is_empty())
        }
        EncoderConfig {
            ffmpeg: var("FFMPEG_PATH").unwrap_or_else(|| "ffmpeg".into()),
            hwaccel_vaapi: var("HWACCEL").is_some_and(|v| v.eq_ignore_ascii_case("vaapi")),
            vaapi_device: var("VAAPI_DEVICE").unwrap_or_else(|| "/dev/dri/renderD128".into()),
            height: var("VIDEO_HEIGHT")
                .and_then(|v| v.parse().ok())
                .unwrap_or(720),
            video_bitrate_k: var("VIDEO_BITRATE_KBPS")
                .and_then(|v| v.parse().ok())
                .unwrap_or(3500),
        }
    }
}

/// The ffmpeg command line: broadcast in (MPEG-2/AC-3 or whatever), H.264/AAC HLS out,
/// with a keyframe every second so every 1 s segment starts cleanly — that's what
/// keeps iPhone start-up fast.
pub fn ffmpeg_args(enc: &EncoderConfig, input_url: &str, out_dir: &Path) -> Vec<String> {
    let h = enc.height;
    let br = enc.video_bitrate_k;
    let mut a: Vec<String> = vec![
        "-nostdin".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
    ];
    if enc.hwaccel_vaapi {
        a.extend(["-vaapi_device".into(), enc.vaapi_device.clone()]);
    }
    a.extend(
        [
            "-fflags",
            "+genpts+discardcorrupt+nobuffer",
            "-probesize",
            "400000",
            "-analyzeduration",
            "400000",
            "-rw_timeout",
            "15000000",
            "-i",
        ]
        .map(String::from),
    );
    a.push(input_url.into());
    a.extend(["-map", "0:v:0", "-map", "0:a:0?", "-sn", "-dn"].map(String::from));
    let scale = format!("yadif=deint=interlaced,scale=-2:'min({h},ih)',fps=30");
    if enc.hwaccel_vaapi {
        a.extend(["-vf".into(), format!("{scale},format=nv12,hwupload")]);
        a.extend(["-c:v", "h264_vaapi", "-profile:v", "main"].map(String::from));
    } else {
        a.extend(["-vf".into(), format!("{scale},format=yuv420p")]);
        a.extend(
            [
                "-c:v",
                "libx264",
                "-preset",
                "veryfast",
                "-tune",
                "zerolatency",
                "-profile:v",
                "main",
            ]
            .map(String::from),
        );
    }
    a.extend([
        "-b:v".into(),
        format!("{br}k"),
        "-maxrate".into(),
        format!("{}k", br + br / 4),
        "-bufsize".into(),
        format!("{}k", br * 2),
        "-g".into(),
        "30".into(),
        "-keyint_min".into(),
        "30".into(),
        "-sc_threshold".into(),
        "0".into(),
        "-force_key_frames".into(),
        "expr:gte(t,n_forced*1)".into(),
    ]);
    a.extend(["-c:a", "aac", "-ac", "2", "-b:a", "128k", "-ar", "48000"].map(String::from));
    a.extend(
        [
            "-f",
            "hls",
            "-hls_time",
            "1",
            "-hls_list_size",
            "8",
            "-hls_flags",
            "delete_segments+independent_segments+omit_endlist+temp_file",
            "-hls_segment_type",
            "mpegts",
            "-hls_segment_filename",
        ]
        .map(String::from),
    );
    a.push(out_dir.join("seg%05d.ts").to_string_lossy().into_owned());
    a.push(out_dir.join("live.m3u8").to_string_lossy().into_owned());
    a
}

/// Mask `user:password@` in any URL inside a log line. ffmpeg echoes the input URL
/// on errors, and that URL carries the tvheadend login.
pub fn redact_credentials(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        let end = tail
            .find(|c: char| c.is_whitespace() || c == '/' || c == '"' || c == '\'')
            .unwrap_or(tail.len());
        match tail[..end].rfind('@') {
            Some(at) => {
                out.push_str("***@");
                rest = &tail[at + 1..];
            }
            None => rest = tail,
        }
    }
    out.push_str(rest);
    out
}

/// Valid file names under `/hls/{id}/{gen}/` — anything else is rejected (no traversal).
pub fn is_hls_file(name: &str) -> bool {
    name == "live.m3u8"
        || (name.starts_with("seg")
            && name.ends_with(".ts")
            && name.len() > 6
            && name[3..name.len() - 3].bytes().all(|b| b.is_ascii_digit()))
}

/// Starts and stops the ffmpeg processes `Hub` asks for.
pub struct Runner {
    pub enc: EncoderConfig,
    /// Unique per process; only paths under here are ever deleted.
    pub root: PathBuf,
    procs: tokio::sync::Mutex<HashMap<u64, (u64, tokio::process::Child)>>,
}

impl Runner {
    pub fn new(enc: EncoderConfig, work_dir: &Path) -> std::io::Result<Self> {
        let root = work_dir.join(format!(
            "run-{}-{}",
            std::process::id(),
            crate::auth::random_token().get(..8).unwrap_or("x")
        ));
        std::fs::create_dir_all(&root)?;
        Ok(Runner {
            enc,
            root,
            procs: tokio::sync::Mutex::new(HashMap::new()),
        })
    }

    pub fn dir(&self, id: u64, generation: u64) -> PathBuf {
        self.root.join(id.to_string()).join(generation.to_string())
    }

    async fn kill(&self, id: u64) {
        let old = self.procs.lock().await.remove(&id);
        if let Some((_, mut child)) = old {
            let _ = child.kill().await;
        }
        let dir = self.root.join(id.to_string());
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    pub async fn apply(
        self: &Arc<Self>,
        hub: &Arc<Mutex<Hub>>,
        input_url: impl Fn(&str) -> String,
        actions: Vec<Action>,
    ) {
        for action in actions {
            match action {
                Action::Stop { id } => self.kill(id).await,
                Action::Start {
                    id,
                    generation,
                    channel,
                } => {
                    self.kill(id).await;
                    let dir = self.dir(id, generation);
                    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
                        hub.lock().unwrap().set_status(
                            id,
                            generation,
                            Status::Error(format!("Can't create work dir: {e}")),
                        );
                        continue;
                    }
                    let args = ffmpeg_args(&self.enc, &input_url(&channel), &dir);
                    let spawned = tokio::process::Command::new(&self.enc.ffmpeg)
                        .args(&args)
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::piped())
                        .kill_on_drop(true)
                        .spawn();
                    match spawned {
                        Ok(mut child) => {
                            let stderr = child.stderr.take();
                            self.procs.lock().await.insert(id, (generation, child));
                            self.clone()
                                .supervise(hub.clone(), id, generation, dir, stderr);
                        }
                        Err(e) => {
                            hub.lock().unwrap().set_status(
                                id,
                                generation,
                                Status::Error(format!("Couldn't run ffmpeg: {e}")),
                            );
                        }
                    }
                }
            }
        }
    }

    /// Flip the stream to Live once the first segment is listed, and to Error if
    /// ffmpeg dies on its own (typically: the tuner didn't deliver any data).
    fn supervise(
        self: Arc<Self>,
        hub: Arc<Mutex<Hub>>,
        id: u64,
        generation: u64,
        dir: PathBuf,
        stderr: Option<tokio::process::ChildStderr>,
    ) {
        let tail: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
        if let Some(err) = stderr {
            let tail = tail.clone();
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(err).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let line = redact_credentials(&line);
                    eprintln!("ffmpeg[{id}/{generation}]: {line}");
                    let mut t = tail.lock().unwrap();
                    t.push_back(line);
                    if t.len() > 6 {
                        t.pop_front();
                    }
                }
            });
        }
        tokio::spawn(async move {
            let playlist = dir.join("live.m3u8");
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                if !hub.lock().unwrap().is_current(id, generation) {
                    return;
                }
                let exited = {
                    let mut procs = self.procs.lock().await;
                    match procs.get_mut(&id) {
                        Some((g, child)) if *g == generation => child.try_wait().ok().flatten(),
                        _ => return,
                    }
                };
                if let Some(code) = exited {
                    let detail = tail
                        .lock()
                        .unwrap()
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(" / ");
                    eprintln!("stream {id}/{generation} ended ({code}): {detail}");
                    hub.lock().unwrap().set_status(
                        id,
                        generation,
                        Status::Error("The tuner isn't sending a picture right now. It usually recovers within a few minutes.".into()),
                    );
                    return;
                }
                let starting = matches!(
                    hub.lock().unwrap().view_status(id, generation),
                    Some(Status::Starting)
                );
                let live = starting
                    && matches!(tokio::fs::read_to_string(&playlist).await, Ok(t) if t.contains(".ts"));
                if live {
                    let mut h = hub.lock().unwrap();
                    if matches!(h.view_status(id, generation), Some(Status::Starting)) {
                        h.set_status(id, generation, Status::Live);
                    }
                }
            }
        });
    }

    pub async fn shutdown(&self) {
        let ids: Vec<u64> = self.procs.lock().await.keys().copied().collect();
        for id in ids {
            self.kill(id).await;
        }
        let _ = tokio::fs::remove_dir_all(&self.root).await;
    }
}
