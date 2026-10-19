//! Who is watching what.
//!
//! A *stream* is one ffmpeg process turning one tvheadend channel into a 1-second-
//! segment HLS playlist. `max_viewers` caps how many streams (i.e. distinct channels,
//! i.e. tuners) may run at once. The browser that started a stream *controls* it;
//! anyone who arrives while every slot is taken *follows* the oldest stream — they
//! see the same picture but can't change the channel. When the controller goes away
//! the longest-waiting follower is promoted, and a stream nobody is watching is
//! stopped after a short grace period (so a page reload doesn't retune the tuner).
//!
//! `Hub` is the pure bookkeeping (unit-tested); it returns `Action`s that `Runner`
//! carries out by starting/stopping ffmpeg.

use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
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
pub struct Slot {
    pub channel: String,
    /// Bumped every time the stream is (re)started; part of the playlist URL so a
    /// browser never mixes segments from two different tunings.
    pub generation: u64,
    pub owner: String,
    pub status: Status,
    created: Instant,
    empty_since: Option<Instant>,
}

#[derive(Clone, Debug)]
struct ClientSlot {
    stream: Option<u64>,
    last_seen: Instant,
    joined: Instant,
    /// Pressed Stop: stay on the guide (don't auto-follow a running stream) until
    /// the tab picks a channel again.
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
    /// Not watching anything yet; free to pick a channel.
    Idle,
    Controller,
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
    /// Other tabs on the same stream.
    pub others: usize,
}

#[derive(Debug, PartialEq)]
pub enum WatchError {
    /// The caller is (now) following someone else's stream.
    Locked,
}

pub struct Hub {
    pub max_streams: u32,
    streams: BTreeMap<u64, Slot>,
    clients: HashMap<String, ClientSlot>,
    /// Tabs signed in as the priority account.
    priority: HashSet<String>,
    next_id: u64,
}

impl Hub {
    pub fn new(max_streams: u32) -> Self {
        Hub {
            max_streams: max_streams.max(1),
            streams: BTreeMap::new(),
            clients: HashMap::new(),
            priority: HashSet::new(),
            next_id: 1,
        }
    }

    fn oldest_stream(&self) -> Option<u64> {
        self.streams
            .iter()
            .min_by_key(|(_, s)| s.created)
            .map(|(id, _)| *id)
    }

    fn at_capacity(&self) -> bool {
        self.streams.len() as u32 >= self.max_streams
    }

    /// Record that `client` is alive. A newcomer arriving while every slot is busy is
    /// attached as a follower straight away, so it shows the shared picture.
    pub fn heartbeat(&mut self, client: &str, now: Instant) {
        let attach_to = if self.at_capacity() {
            self.oldest_stream()
        } else {
            None
        };
        let c = self
            .clients
            .entry(client.to_string())
            .or_insert(ClientSlot {
                stream: None,
                last_seen: now,
                joined: now,
                parked: false,
            });
        c.last_seen = now;
        if c.stream.is_none() && !c.parked {
            c.stream = attach_to;
        }
        if let Some(id) = c.stream
            && let Some(s) = self.streams.get_mut(&id)
        {
            s.empty_since = None;
        }
    }

    /// `client` asks to watch `channel`. `retry` restarts the same channel after an error.
    /// Mark whether `client` belongs to the priority account. Called on every
    /// request, so changing an account's priority takes effect immediately.
    pub fn mark_priority(&mut self, client: &str, priority: bool) {
        if priority {
            self.priority.insert(client.to_string());
        } else {
            self.priority.remove(client);
        }
    }

    fn is_priority(&self, client: &str) -> bool {
        self.priority.contains(client)
    }

    /// Point stream `id` at `channel`. Same channel = no retune (unless retrying a
    /// failed start); otherwise one restart — the Runner kills the old ffmpeg before
    /// starting the new one, so the tuner is never asked for two muxes at once.
    fn steer(&mut self, id: u64, channel: &str, retry: bool) -> Vec<Action> {
        let s = self.streams.get_mut(&id).expect("live stream");
        let failed = matches!(s.status, Status::Error(_));
        if s.channel == channel && !(retry && failed) {
            return vec![];
        }
        s.channel = channel.to_string();
        s.generation += 1;
        s.status = Status::Starting;
        vec![Action::Start {
            id,
            generation: s.generation,
            channel: channel.to_string(),
        }]
    }

    /// `client` asks to watch `channel`. `retry` restarts the same channel after an error.
    ///
    /// The priority viewer may take control of a stream run by a non-priority viewer;
    /// the bumped viewer stays on as a follower (same picture, lock banner).
    pub fn watch(
        &mut self,
        client: &str,
        channel: &str,
        retry: bool,
        now: Instant,
    ) -> Result<Vec<Action>, WatchError> {
        if let Some(c) = self.clients.get_mut(client) {
            c.parked = false;
        }
        self.heartbeat(client, now);
        let priority = self.is_priority(client);
        let current = self.clients.get(client).and_then(|c| c.stream);
        if let Some(id) = current {
            let owner = self.streams[&id].owner.clone();
            if owner != client {
                if !priority || self.is_priority(&owner) {
                    return Err(WatchError::Locked);
                }
                self.streams.get_mut(&id).unwrap().owner = client.to_string();
            }
            return Ok(self.steer(id, channel, retry));
        }
        if self.at_capacity() {
            if priority {
                // Take over the oldest stream not already run by the priority viewer.
                let target = self
                    .streams
                    .iter()
                    .filter(|(_, s)| !self.is_priority(&s.owner))
                    .min_by_key(|(_, s)| s.created)
                    .map(|(id, _)| *id);
                if let Some(id) = target {
                    self.streams.get_mut(&id).unwrap().owner = client.to_string();
                    self.clients.get_mut(client).unwrap().stream = Some(id);
                    return Ok(self.steer(id, channel, retry));
                }
            }
            let oldest = self.oldest_stream();
            if let Some(c) = self.clients.get_mut(client) {
                c.stream = oldest;
            }
            return Err(WatchError::Locked);
        }
        let id = self.next_id;
        self.next_id += 1;
        self.streams.insert(
            id,
            Slot {
                channel: channel.to_string(),
                generation: 1,
                owner: client.to_string(),
                status: Status::Starting,
                created: now,
                empty_since: None,
            },
        );
        self.clients
            .get_mut(client)
            .expect("heartbeat inserted it")
            .stream = Some(id);
        Ok(vec![Action::Start {
            id,
            generation: 1,
            channel: channel.to_string(),
        }])
    }

    /// The Stop button: detach `client` and keep it parked on the guide. If nobody
    /// else is watching, the stream (and the tuner) is released immediately rather
    /// than after the idle grace period; otherwise the next viewer takes control.
    pub fn stop(&mut self, client: &str, now: Instant) -> Vec<Action> {
        self.heartbeat(client, now);
        let c = self.clients.get_mut(client).expect("heartbeat inserted it");
        c.parked = true;
        let Some(id) = c.stream.take() else {
            return vec![];
        };
        let mut watchers: Vec<(&String, &ClientSlot)> = self
            .clients
            .iter()
            .filter(|(_, c)| c.stream == Some(id))
            .collect();
        // A waiting priority viewer is handed control first.
        watchers.sort_by_key(|(name, c)| (!self.priority.contains(*name), c.joined));
        if watchers.is_empty() {
            self.streams.remove(&id);
            return vec![Action::Stop { id }];
        }
        let next = watchers[0].0.clone();
        if let Some(s) = self.streams.get_mut(&id)
            && s.owner == client
        {
            s.owner = next;
        }
        vec![]
    }

    pub fn leave(&mut self, client: &str, now: Instant) -> Vec<Action> {
        self.clients.remove(client);
        self.tick(now)
    }

    /// Housekeeping: drop silent clients, hand control to followers, stop idle streams.
    pub fn tick(&mut self, now: Instant) -> Vec<Action> {
        self.clients
            .retain(|_, c| now.duration_since(c.last_seen) < CLIENT_TIMEOUT);
        let clients = &self.clients;
        self.priority.retain(|c| clients.contains_key(c));
        let mut actions = Vec::new();
        let ids: Vec<u64> = self.streams.keys().copied().collect();
        for id in ids {
            let mut watchers: Vec<(&String, &ClientSlot)> = self
                .clients
                .iter()
                .filter(|(_, c)| c.stream == Some(id))
                .collect();
            // A waiting priority viewer is handed control first.
            watchers.sort_by_key(|(name, c)| (!self.priority.contains(*name), c.joined));
            let s = self.streams.get_mut(&id).unwrap();
            if watchers.is_empty() {
                let since = *s.empty_since.get_or_insert(now);
                if now.duration_since(since) >= IDLE_GRACE {
                    self.streams.remove(&id);
                    actions.push(Action::Stop { id });
                }
                continue;
            }
            s.empty_since = None;
            if !watchers.iter().any(|(name, _)| **name == s.owner) {
                s.owner = watchers[0].0.clone();
            }
        }
        actions
    }

    pub fn set_status(&mut self, id: u64, generation: u64, status: Status) {
        if let Some(s) = self.streams.get_mut(&id)
            && s.generation == generation
        {
            s.status = status;
        }
    }

    pub fn is_current(&self, id: u64, generation: u64) -> bool {
        self.streams
            .get(&id)
            .is_some_and(|s| s.generation == generation)
    }

    pub fn view(&self, client: &str) -> View {
        let stream_id = self.clients.get(client).and_then(|c| c.stream);
        let Some((id, s)) = stream_id.and_then(|id| self.streams.get(&id).map(|s| (id, s))) else {
            return View {
                role: Role::Idle,
                stream: None,
                others: 0,
            };
        };
        let others = self
            .clients
            .iter()
            .filter(|(k, c)| c.stream == Some(id) && k.as_str() != client)
            .count();
        View {
            role: if s.owner == client {
                Role::Controller
            } else {
                Role::Follower
            },
            stream: Some(StreamView {
                id,
                generation: s.generation,
                channel: s.channel.clone(),
                status: s.status.clone(),
                playlist: format!("/hls/{id}/{}/live.m3u8", s.generation),
            }),
            others,
        }
    }

    /// Change the cap. Running streams beyond a lowered cap keep going until they end.
    pub fn set_max(&mut self, max: u32) {
        self.max_streams = max.max(1);
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

impl Hub {
    fn view_status(&self, id: u64, generation: u64) -> Option<Status> {
        self.streams
            .get(&id)
            .filter(|s| s.generation == generation)
            .map(|s| s.status.clone())
    }

    pub fn view_status_pub(&self, id: u64) -> Option<Status> {
        self.streams.get(&id).map(|s| s.status.clone())
    }
}
