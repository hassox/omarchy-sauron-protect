//! `sauron watch`: the long-running bridge between Protect and the shell plugin.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{Mutex, mpsc};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{Instant, Interval, MissedTickBehavior, interval, sleep, sleep_until};
use tokio_tungstenite::tungstenite::{Bytes, Message};

use crate::config::{self, Config};
use crate::live;
use crate::notify::{Notification, Notifier};
use crate::proto::{CameraOut, Command, Level, Msg, Out, PROTOCOL, Phase, State};
use crate::protect::{self, Camera, DeviceItem, EventItem, Frame, Protect, Streams, WsStream};

const CONFIG_POLL: Duration = Duration::from_secs(3);
const SWEEP_EVERY: Duration = Duration::from_secs(60);
const EVENT_TTL: Duration = Duration::from_secs(30 * 60);
const PING_EVERY: Duration = Duration::from_secs(60);
const DEAD_AFTER: Duration = Duration::from_secs(150);
const BACKOFF_MIN: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
const AUTH_BACKOFF: Duration = Duration::from_secs(60);
const EVENT_IMAGES_KEPT: usize = 50;
const ENDED_REMEMBERED: usize = 256;
const EVENT_TYPES: [&str; 6] =
    ["ring", "motion", "smartDetectZone", "smartDetectLine", "smartDetectLoiterZone", "smartAudioDetect"];

/// Messages from spawned tasks to the main loop. `generation` tags data from a connection
/// session so results from a superseded config are dropped.
enum Internal {
    Stdin(Vec<u8>),
    StdinClosed,
    Status { generation: u64, state: State, message: Option<String>, protect: Option<String> },
    Cameras { generation: u64, cameras: Vec<Camera> },
    Streams { generation: u64, camera: String, streams: Streams },
    Event { generation: u64, action: String, item: EventItem },
    CameraPatch { generation: u64, item: DeviceItem },
    Disconnected { generation: u64 },
    Snapshot { camera: String, tracked: bool, result: Result<u64, String> },
    PlayerSpawned { generation: u64, camera: String, pid: u32, created: Option<String> },
    PlayerFailed { camera: String, message: String },
    PlayerExited { camera: String, pid: u32 },
    Focused { camera: String, result: Result<(), String> },
    Log(Level, String),
}

type Tx = mpsc::UnboundedSender<Internal>;

/// State shared with spawned snapshot/notification tasks.
struct Shared {
    snap_dir: PathBuf,
    events_dir: PathBuf,
    seq: AtomicU64,
    /// Serializes snapshot file writes and their seq assignment.
    write_lock: Mutex<()>,
    notifier: Notifier,
}

struct Cam {
    info: Camera,
    kinds: Vec<String>,
    seq: u64,
    snapshot: String,
}

/// The fields of a camera that, when changed, re-emit the `cameras` list.
#[derive(PartialEq)]
struct CamView {
    id: String,
    name: String,
    model: String,
    online: bool,
    kinds: Vec<String>,
}

struct OpenEvent {
    camera: String,
    event_type: String,
    kinds: Vec<String>,
    start: i64,
    seen: Instant,
}

struct Filter {
    notify: Vec<String>,
    cameras: Vec<String>,
    desktop: bool,
}

impl Filter {
    fn camera_passes(&self, camera: &str) -> bool {
        self.cameras.is_empty() || self.cameras.iter().any(|c| c == camera)
    }

    fn matches(&self, camera: &str, kinds: &[String]) -> bool {
        self.camera_passes(camera) && kinds.iter().any(|k| self.notify.contains(k))
    }
}

enum Player {
    Starting,
    Running(u32),
}

struct Session {
    config: Config,
    client: Arc<Protect>,
    task: JoinHandle<()>,
}

struct Daemon {
    out: Out,
    tx: Tx,
    shared: Arc<Shared>,
    config_path: PathBuf,
    config_mtime: Option<SystemTime>,
    generation: u64,
    session: Option<Session>,
    status: (State, Option<String>, Option<String>),
    cameras: HashMap<String, Cam>,
    emitted_cameras: Option<Vec<CamView>>,
    streams: HashMap<String, Streams>,
    events: HashMap<String, OpenEvent>,
    ended: VecDeque<String>,
    filter: Filter,
    watch: Option<Interval>,
    in_flight: HashSet<String>,
    snapshot_errors: HashMap<String, String>,
    players: HashMap<String, Player>,
}

enum Wake {
    Internal(Internal),
    WatchTick,
    ConfigPoll,
    Sweep,
    Signal,
}

pub async fn run() -> Result<()> {
    let runtime = config::runtime_dir();
    let snap_dir = runtime.join("snap");
    let events_dir = runtime.join("events");
    create_runtime_dirs(&runtime, &snap_dir, &events_dir).await?;
    let exe = std::env::current_exe().context("cannot locate own executable")?;

    let (tx, mut rx) = mpsc::unbounded_channel();
    tokio::spawn(read_stdin(tx.clone()));

    let mut daemon = Daemon {
        out: Out::new(),
        tx,
        shared: Arc::new(Shared {
            snap_dir,
            events_dir,
            seq: AtomicU64::new(0),
            write_lock: Mutex::new(()),
            notifier: Notifier::new(exe.to_string_lossy().into_owned()),
        }),
        config_path: config::config_path(),
        config_mtime: None,
        generation: 0,
        session: None,
        status: (State::Connecting, None, None),
        cameras: HashMap::new(),
        emitted_cameras: None,
        streams: HashMap::new(),
        events: HashMap::new(),
        ended: VecDeque::with_capacity(ENDED_REMEMBERED),
        filter: Filter {
            notify: ["person", "vehicle", "package", "ring"].map(String::from).to_vec(),
            cameras: Vec::new(),
            desktop: true,
        },
        watch: None,
        in_flight: HashSet::new(),
        snapshot_errors: HashMap::new(),
        players: HashMap::new(),
    };

    emit(&mut daemon.out, &Msg::Hello { version: env!("CARGO_PKG_VERSION"), protocol: PROTOCOL }).await;
    daemon.reload().await;

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut config_poll = interval(CONFIG_POLL);
    config_poll.set_missed_tick_behavior(MissedTickBehavior::Delay);
    config_poll.reset();
    let mut sweep = interval(SWEEP_EVERY);
    sweep.reset();

    loop {
        let wake = tokio::select! {
            Some(msg) = rx.recv() => Wake::Internal(msg),
            () = tick(&mut daemon.watch) => Wake::WatchTick,
            _ = config_poll.tick() => Wake::ConfigPoll,
            _ = sweep.tick() => Wake::Sweep,
            _ = sigterm.recv() => Wake::Signal,
            _ = sigint.recv() => Wake::Signal,
        };
        match wake {
            Wake::Signal => std::process::exit(0),
            Wake::Internal(msg) => daemon.handle(msg).await,
            Wake::WatchTick => daemon.watch_tick(),
            Wake::ConfigPoll => {
                if config_mtime(&daemon.config_path).await != daemon.config_mtime {
                    daemon.reload().await;
                }
            }
            Wake::Sweep => daemon.sweep().await,
        }
    }
}

async fn create_runtime_dirs(root: &Path, snap: &Path, events: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(root)
        .await
        .with_context(|| format!("cannot create {}", root.display()))?;
    tokio::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).await?;
    for dir in [snap, events] {
        tokio::fs::create_dir_all(dir).await.with_context(|| format!("cannot create {}", dir.display()))?;
    }
    Ok(())
}

async fn tick(watch: &mut Option<Interval>) {
    match watch {
        Some(interval) => {
            interval.tick().await;
        }
        None => std::future::pending().await,
    }
}

async fn config_mtime(path: &Path) -> Option<SystemTime> {
    tokio::fs::metadata(path).await.ok()?.modified().ok()
}

/// Writes one message; a closed stdout means the parent is gone, so exit.
async fn emit(out: &mut Out, msg: &Msg<'_>) {
    if out.send(msg).await.is_err() {
        std::process::exit(0);
    }
}

async fn read_stdin(tx: Tx) {
    let mut stdin = BufReader::new(tokio::io::stdin());
    loop {
        let mut line = Vec::with_capacity(256);
        match stdin.read_until(b'\n', &mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if tx.send(Internal::Stdin(line)).is_err() {
                    return;
                }
            }
        }
    }
    let _ = tx.send(Internal::StdinClosed);
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// Kinds of an event, normalized for filtering and display.
fn event_kinds(event_type: &str, smart_detect_types: Option<&[String]>) -> Vec<String> {
    match event_type {
        "ring" => vec!["ring".into()],
        "motion" => vec!["motion".into()],
        "smartAudioDetect" => vec!["audio".into()],
        _ => smart_detect_types.map(<[String]>::to_vec).unwrap_or_default(),
    }
}

impl Daemon {
    async fn log(&mut self, level: Level, message: &str) {
        emit(&mut self.out, &Msg::Log { level, message }).await;
    }

    async fn set_status(&mut self, state: State, message: Option<String>, protect: Option<String>) {
        let status = (state, message, protect);
        if status == self.status {
            return;
        }
        self.status = status;
        let (state, message, protect) = &self.status;
        emit(&mut self.out, &Msg::Status { state: *state, message: message.as_deref(), protect: protect.as_deref() })
            .await;
    }

    fn online(&self) -> bool {
        self.status.0 == State::Online
    }

    /// Re-reads the config and restarts the connection session.
    async fn reload(&mut self) {
        if let Some(session) = self.session.take() {
            session.task.abort();
        }
        self.generation += 1;
        self.force_end_all().await;
        self.config_mtime = config_mtime(&self.config_path).await;

        let started = match config::load(&self.config_path).await {
            Ok(config) => Protect::new(&config).map(|client| (config, Arc::new(client))),
            Err(err) => Err(err),
        };
        match started {
            Ok((config, client)) => {
                self.set_status(State::Connecting, None, None).await;
                let task = tokio::spawn(run_session(self.generation, client.clone(), self.tx.clone()));
                self.session = Some(Session { config, client, task });
            }
            Err(err) => {
                self.set_status(State::Unconfigured, Some(format!("{err:#}")), None).await;
                self.cameras.clear();
                self.streams.clear();
                self.emit_cameras().await;
            }
        }
    }

    async fn handle(&mut self, msg: Internal) {
        match msg {
            Internal::Stdin(line) => self.command(&line).await,
            // The parent shell is gone.
            Internal::StdinClosed => std::process::exit(0),
            Internal::Status { generation, state, message, protect } if generation == self.generation => {
                self.set_status(state, message, protect).await;
            }
            Internal::Cameras { generation, cameras } if generation == self.generation => {
                self.set_cameras(cameras).await;
            }
            Internal::Streams { generation, camera, streams } if generation == self.generation => {
                self.streams.insert(camera, streams);
            }
            Internal::Event { generation, action, item } if generation == self.generation => {
                self.on_event(&action, item).await;
            }
            Internal::CameraPatch { generation, item } if generation == self.generation => {
                self.patch_cameras(item).await;
            }
            Internal::Disconnected { generation } if generation == self.generation => self.force_end_all().await,
            Internal::Snapshot { camera, tracked, result } => self.on_snapshot(camera, tracked, result).await,
            Internal::PlayerSpawned { generation, camera, pid, created } => {
                if let (Some(url), true) = (created, generation == self.generation) {
                    let quality = self.session.as_ref().map(|s| s.config.live_quality.as_str()).unwrap_or("high");
                    self.streams.entry(camera.clone()).or_default().set(quality, url);
                }
                self.players.insert(camera.clone(), Player::Running(pid));
                self.live_result(&camera, true, None).await;
            }
            Internal::PlayerFailed { camera, message } => {
                if matches!(self.players.get(&camera), Some(Player::Starting)) {
                    self.players.remove(&camera);
                }
                self.live_result(&camera, false, Some(&message)).await;
            }
            Internal::PlayerExited { camera, pid } => {
                if matches!(self.players.get(&camera), Some(Player::Running(p)) if *p == pid) {
                    self.players.remove(&camera);
                }
            }
            Internal::Focused { camera, result } => {
                self.live_result(&camera, result.is_ok(), result.err().as_deref()).await;
            }
            Internal::Log(level, message) => self.log(level, &message).await,
            // Stale data from a superseded session.
            Internal::Status { .. }
            | Internal::Cameras { .. }
            | Internal::Streams { .. }
            | Internal::Event { .. }
            | Internal::CameraPatch { .. }
            | Internal::Disconnected { .. } => {}
        }
    }

    async fn command(&mut self, line: &[u8]) {
        if line.trim_ascii().is_empty() {
            return;
        }
        let command = match serde_json::from_slice::<Command>(line) {
            Ok(command) => command,
            Err(err) => {
                let text = String::from_utf8_lossy(line.trim_ascii());
                return self.log(Level::Warn, &format!("ignoring invalid command {text}: {err}")).await;
            }
        };
        match command {
            Command::Filter { notify, cameras, desktop } => {
                if let Some(notify) = notify {
                    self.filter.notify = notify;
                }
                if let Some(cameras) = cameras {
                    self.filter.cameras = cameras;
                }
                if let Some(desktop) = desktop {
                    self.filter.desktop = desktop;
                }
            }
            Command::Watch { on: false, .. } => self.watch = None,
            Command::Watch { on: true, interval: secs } => {
                let secs = secs.filter(|s| s.is_finite()).unwrap_or(2.0).clamp(1.0, 60.0);
                let mut watch = interval(Duration::from_secs_f64(secs));
                watch.set_missed_tick_behavior(MissedTickBehavior::Delay);
                self.watch = Some(watch);
            }
            Command::Snapshot { camera } => {
                if self.cameras.contains_key(&camera) {
                    self.fetch_snapshot(&camera);
                } else {
                    self.log(Level::Warn, &format!("snapshot: unknown camera {camera}")).await;
                }
            }
            Command::Live { camera } => self.live(camera).await,
            Command::Reload => self.reload().await,
        }
    }

    // ---- cameras -------------------------------------------------------------------------

    async fn set_cameras(&mut self, cameras: Vec<Camera>) {
        let mut previous = std::mem::take(&mut self.cameras);
        for info in cameras {
            let seq = previous.remove(&info.id).map_or(0, |cam| cam.seq);
            let snapshot = self.shared.snap_dir.join(format!("{}.jpg", info.id)).to_string_lossy().into_owned();
            let kinds = info.kinds();
            self.cameras.insert(info.id.clone(), Cam { info, kinds, seq, snapshot });
        }
        self.streams.retain(|id, _| self.cameras.contains_key(id));
        self.emit_cameras().await;
    }

    async fn patch_cameras(&mut self, item: DeviceItem) {
        for id in item.id.as_slice() {
            if let Some(cam) = self.cameras.get_mut(id) {
                if let Some(name) = &item.name {
                    cam.info.name = Some(name.clone());
                }
                if let Some(state) = &item.state {
                    cam.info.state = Some(state.clone());
                }
            }
        }
        self.emit_cameras().await;
    }

    /// Emits the camera list (sorted by name) if anything but snapshot seqs changed.
    async fn emit_cameras(&mut self) {
        let mut cams: Vec<&Cam> = self.cameras.values().collect();
        cams.sort_by_cached_key(|cam| (cam.info.display_name().to_lowercase(), cam.info.id.clone()));
        let views: Vec<CamView> = cams
            .iter()
            .map(|cam| CamView {
                id: cam.info.id.clone(),
                name: cam.info.display_name().to_owned(),
                model: cam.info.model().to_owned(),
                online: cam.info.online(),
                kinds: cam.kinds.clone(),
            })
            .collect();
        if self.emitted_cameras.as_ref() == Some(&views) {
            return;
        }
        let cameras = views
            .iter()
            .zip(&cams)
            .map(|(view, cam)| CameraOut {
                id: &view.id,
                name: &view.name,
                model: &view.model,
                online: view.online,
                kinds: &view.kinds,
                snapshot: &cam.snapshot,
                seq: cam.seq,
            })
            .collect();
        emit(&mut self.out, &Msg::Cameras { cameras }).await;
        self.emitted_cameras = Some(views);
    }

    // ---- snapshots -----------------------------------------------------------------------

    fn watch_tick(&mut self) {
        if !self.online() {
            return;
        }
        let online: Vec<String> =
            self.cameras.values().filter(|cam| cam.info.online()).map(|cam| cam.info.id.clone()).collect();
        for camera in online {
            self.fetch_snapshot(&camera);
        }
    }

    /// Starts a tracked low-quality refresh unless one is already in flight for the camera.
    fn fetch_snapshot(&mut self, camera: &str) {
        let Some(session) = &self.session else { return };
        if !self.in_flight.insert(camera.to_owned()) {
            return;
        }
        let (shared, client, tx, camera) = (self.shared.clone(), session.client.clone(), self.tx.clone(), camera.to_owned());
        tokio::spawn(async move {
            let result = store_snapshot(&shared, &client, &camera, None, &tx, true).await;
            if let Err(err) = result {
                let _ = tx.send(Internal::Snapshot { camera, tracked: true, result: Err(format!("{err:#}")) });
            }
        });
    }

    async fn on_snapshot(&mut self, camera: String, tracked: bool, result: Result<u64, String>) {
        if tracked {
            self.in_flight.remove(&camera);
        }
        let name = self.cameras.get(&camera).map_or(camera.as_str(), |cam| cam.info.display_name()).to_owned();
        match result {
            Ok(seq) => {
                self.snapshot_errors.remove(&camera);
                let Some(cam) = self.cameras.get_mut(&camera) else { return };
                cam.seq = seq;
                emit(&mut self.out, &Msg::Snapshot { camera: &camera, seq, path: &cam.snapshot }).await;
            }
            Err(err) => {
                if self.snapshot_errors.get(&camera) != Some(&err) {
                    self.log(Level::Warn, &format!("snapshot {name}: {err}")).await;
                    self.snapshot_errors.insert(camera, err);
                }
            }
        }
    }

    // ---- events --------------------------------------------------------------------------

    async fn on_event(&mut self, action: &str, item: EventItem) {
        let Some(id) = item.id else { return };
        if self.ended.contains(&id) {
            return;
        }
        if let Some(open) = self.events.get_mut(&id) {
            let old_kinds = match item.smart_detect_types.as_deref() {
                Some(types) => {
                    let kinds = event_kinds(&open.event_type, Some(types));
                    (kinds != open.kinds).then(|| std::mem::replace(&mut open.kinds, kinds))
                }
                None => None,
            };
            if let Some(old_kinds) = old_kinds {
                self.emit_event(Phase::Update, &id, None).await;
                let open = &self.events[&id];
                let grew = self.filter.camera_passes(&open.camera)
                    && open.kinds.iter().any(|k| self.filter.notify.contains(k) && !old_kinds.contains(k));
                if grew {
                    self.event_actions(&id);
                }
            }
            if let Some(end) = item.end {
                self.end_event(&id, end).await;
            }
            return;
        }

        // Unseen id: an `add`, or a born-closed `update` carrying the complete event.
        let (Some(event_type), Some(start), Some(camera)) = (item.kind, item.start, item.device) else { return };
        if action == "update" && item.end.is_none() || !matches!(action, "add" | "update") {
            return;
        }
        if !EVENT_TYPES.contains(&event_type.as_str()) || !self.cameras.contains_key(&camera) {
            return;
        }
        let kinds = event_kinds(&event_type, item.smart_detect_types.as_deref());
        self.events.insert(id.clone(), OpenEvent { camera, event_type, kinds, start, seen: Instant::now() });
        self.emit_event(Phase::Start, &id, None).await;
        let open = &self.events[&id];
        if self.filter.matches(&open.camera, &open.kinds) {
            self.event_actions(&id);
        }
        if let Some(end) = item.end {
            self.end_event(&id, end).await;
        }
    }

    async fn emit_event(&mut self, phase: Phase, id: &str, end: Option<i64>) {
        let Some(open) = self.events.get(id) else { return };
        let msg = Msg::Event {
            phase,
            id,
            camera: &open.camera,
            event_type: &open.event_type,
            kinds: &open.kinds,
            matched: self.filter.matches(&open.camera, &open.kinds),
            start: open.start,
            end,
        };
        emit(&mut self.out, &msg).await;
    }

    async fn end_event(&mut self, id: &str, end: i64) {
        self.emit_event(Phase::End, id, Some(end)).await;
        self.events.remove(id);
        if self.ended.len() == ENDED_REMEMBERED {
            self.ended.pop_front();
        }
        self.ended.push_back(id.to_owned());
    }

    async fn force_end_all(&mut self) {
        let ids: Vec<String> = self.events.keys().cloned().collect();
        let now = now_ms();
        for id in ids {
            self.end_event(&id, now).await;
        }
    }

    async fn sweep(&mut self) {
        let stale: Vec<String> =
            self.events.iter().filter(|(_, e)| e.seen.elapsed() > EVENT_TTL).map(|(id, _)| id.clone()).collect();
        let now = now_ms();
        for id in stale {
            self.end_event(&id, now).await;
        }
    }

    /// Snapshot to `snap/` + `events/`, then the desktop notification; runs detached so the
    /// `event` line is never delayed.
    fn event_actions(&self, id: &str) {
        let (Some(session), Some(open)) = (&self.session, self.events.get(id)) else { return };
        let camera_name = self.cameras.get(&open.camera).map_or(open.camera.as_str(), |c| c.info.display_name());
        let job = EventJob {
            event_id: id.to_owned(),
            camera: open.camera.clone(),
            camera_name: camera_name.to_owned(),
            kinds: open.kinds.clone(),
            start: open.start,
            desktop: self.filter.desktop,
        };
        tokio::spawn(run_event_job(self.shared.clone(), session.client.clone(), self.tx.clone(), job));
    }

    // ---- live ----------------------------------------------------------------------------

    async fn live_result(&mut self, camera: &str, ok: bool, message: Option<&str>) {
        emit(&mut self.out, &Msg::Live { camera, ok, message }).await;
    }

    async fn live(&mut self, camera: String) {
        let (Some(session), Some(cam)) = (&self.session, self.cameras.get(&camera)) else {
            let message = if self.session.is_none() { "not configured" } else { "unknown camera" };
            return self.live_result(&camera, false, Some(message)).await;
        };
        match self.players.get(&camera) {
            Some(Player::Running(pid)) => {
                let (pid, tx) = (*pid, self.tx.clone());
                tokio::spawn(async move {
                    let result = live::focus(pid).await.map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Internal::Focused { camera, result });
                });
            }
            Some(Player::Starting) => self.live_result(&camera, true, Some("player is starting")).await,
            None => {
                let quality = session.config.live_quality.clone();
                let job = LiveJob {
                    generation: self.generation,
                    cached: self.streams.get(&camera).and_then(|s| s.get(&quality)).map(str::to_owned),
                    quality,
                    player: session.config.player.clone(),
                    camera_name: cam.info.display_name().to_owned(),
                    camera: camera.clone(),
                };
                tokio::spawn(run_live_job(session.client.clone(), self.tx.clone(), job));
                self.players.insert(camera, Player::Starting);
            }
        }
    }
}

// ---- detached jobs ---------------------------------------------------------------------------

struct EventJob {
    event_id: String,
    camera: String,
    camera_name: String,
    kinds: Vec<String>,
    start: i64,
    desktop: bool,
}

async fn run_event_job(shared: Arc<Shared>, client: Arc<Protect>, tx: Tx, job: EventJob) {
    let image = match store_snapshot(&shared, &client, &job.camera, Some(&job.event_id), &tx, false).await {
        Ok(path) => {
            if let Err(err) = prune_event_images(&shared.events_dir).await {
                let _ = tx.send(Internal::Log(Level::Error, format!("pruning event images: {err:#}")));
            }
            path
        }
        Err(err) => {
            let result = Err(format!("{err:#}"));
            let _ = tx.send(Internal::Snapshot { camera: job.camera.clone(), tracked: false, result });
            None
        }
    };
    if !job.desktop {
        return;
    }
    let notification = Notification {
        event_id: &job.event_id,
        camera_id: &job.camera,
        camera_name: &job.camera_name,
        kinds: &job.kinds,
        start_ms: job.start,
        image: image.as_deref(),
    };
    if let Err(err) = shared.notifier.notify(&notification).await {
        let _ = tx.send(Internal::Log(Level::Warn, format!("notification: {err:#}")));
    }
}

/// Fetches a low-quality snapshot and atomically replaces `snap/<camera>.jpg` (plus
/// `events/<event>.jpg` if given), reporting the new seq. Returns the event image path.
async fn store_snapshot(
    shared: &Shared,
    client: &Protect,
    camera: &str,
    event: Option<&str>,
    tx: &Tx,
    tracked: bool,
) -> Result<Option<PathBuf>> {
    let is_safe = |id: &str| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !is_safe(camera) || !event.is_none_or(is_safe) {
        bail!("refusing unsafe id in file name");
    }
    let jpeg = client.snapshot(camera, false).await?;

    let _guard = shared.write_lock.lock().await;
    write_atomic(&shared.snap_dir.join(format!("{camera}.jpg")), &jpeg).await?;
    let event_image = match event {
        Some(event) => {
            let path = shared.events_dir.join(format!("{event}.jpg"));
            write_atomic(&path, &jpeg).await?;
            Some(path)
        }
        None => None,
    };
    let seq = shared.seq.fetch_add(1, Ordering::Relaxed) + 1;
    let _ = tx.send(Internal::Snapshot { camera: camera.to_owned(), tracked, result: Ok(seq) });
    Ok(event_image)
}

async fn write_atomic(path: &Path, data: &Bytes) -> Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    tokio::fs::write(&tmp, data).await.with_context(|| format!("cannot write {}", path.display()))?;
    tokio::fs::rename(&tmp, path).await.with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(())
}

/// Keeps only the newest event images.
async fn prune_event_images(dir: &Path) -> Result<()> {
    let mut entries = tokio::fs::read_dir(dir).await?;
    let mut images = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "jpg") {
            let modified = entry.metadata().await?.modified()?;
            images.push((modified, path));
        }
    }
    if images.len() <= EVENT_IMAGES_KEPT {
        return Ok(());
    }
    images.sort_unstable_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    for (_, path) in images.drain(EVENT_IMAGES_KEPT..) {
        match tokio::fs::remove_file(&path).await {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => return Err(err.into()),
            _ => {}
        }
    }
    Ok(())
}

struct LiveJob {
    generation: u64,
    camera: String,
    camera_name: String,
    quality: String,
    cached: Option<String>,
    player: Vec<String>,
}

async fn run_live_job(client: Arc<Protect>, tx: Tx, job: LiveJob) {
    let spawned = async {
        let (url, created) = match job.cached {
            Some(url) => (url, None),
            None => {
                let url = client.create_stream(&job.camera, &job.quality).await?;
                (url.clone(), Some(url))
            }
        };
        let url = protect::fixup_stream_url(&url, client.hostname())?;
        let child = live::spawn_player(&job.player, &job.camera_name, &url)?;
        let pid = child.id().ok_or_else(|| anyhow!("player exited immediately"))?;
        anyhow::Ok((child, pid, created))
    };
    match spawned.await {
        Ok((mut child, pid, created)) => {
            let camera = job.camera;
            let _ = tx.send(Internal::PlayerSpawned { generation: job.generation, camera: camera.clone(), pid, created });
            let _ = child.wait().await;
            let _ = tx.send(Internal::PlayerExited { camera, pid });
        }
        Err(err) => {
            let _ = tx.send(Internal::PlayerFailed { camera: job.camera, message: format!("{err:#}") });
        }
    }
}

// ---- connection session ----------------------------------------------------------------------

/// Connects, streams both websockets and reconnects with backoff until aborted.
async fn run_session(generation: u64, client: Arc<Protect>, tx: Tx) {
    let mut backoff = BACKOFF_MIN;
    loop {
        let err = match session_once(generation, &client, &tx, &mut backoff).await {
            Ok(never) => match never {},
            Err(err) => err,
        };
        let _ = tx.send(Internal::Disconnected { generation });
        let (state, wait) = if protect::is_auth(&err) {
            (State::Auth, AUTH_BACKOFF)
        } else {
            let wait = backoff;
            backoff = (backoff * 2).min(BACKOFF_MAX);
            (State::Offline, wait)
        };
        let _ = tx.send(Internal::Status { generation, state, message: Some(format!("{err:#}")), protect: None });
        sleep(wait).await;
    }
}

async fn session_once(
    generation: u64,
    client: &Arc<Protect>,
    tx: &Tx,
    backoff: &mut Duration,
) -> Result<std::convert::Infallible> {
    let version = client.version().await?;
    let cameras = client.cameras().await?;
    let (mut events, mut devices) = tokio::try_join!(client.subscribe("events"), client.subscribe("devices"))?;

    let mut background = JoinSet::new();
    background.spawn(fetch_streams(generation, client.clone(), tx.clone(), camera_ids(&cameras)));
    let _ = tx.send(Internal::Cameras { generation, cameras });
    let _ = tx.send(Internal::Status { generation, state: State::Online, message: None, protect: Some(version) });
    *backoff = BACKOFF_MIN;

    let mut ping = tokio::time::interval_at(Instant::now() + PING_EVERY, PING_EVERY);
    let (mut events_seen, mut devices_seen) = (Instant::now(), Instant::now());
    loop {
        tokio::select! {
            frame = events.next() => {
                events_seen = Instant::now();
                if let Some(text) = text_frame("events", frame)? {
                    match serde_json::from_str::<Frame<EventItem>>(&text) {
                        Ok(frame) => {
                            let _ = tx.send(Internal::Event { generation, action: frame.action, item: frame.item });
                        }
                        Err(err) => { let _ = tx.send(Internal::Log(Level::Warn, format!("unparsable event frame: {err}"))); }
                    }
                }
            }
            frame = devices.next() => {
                devices_seen = Instant::now();
                if let Some(text) = text_frame("devices", frame)? {
                    match serde_json::from_str::<Frame<DeviceItem>>(&text) {
                        Ok(frame) if frame.item.model_key.as_deref() == Some("camera") => match frame.action.as_str() {
                            "update" => { let _ = tx.send(Internal::CameraPatch { generation, item: frame.item }); }
                            "add" | "remove" => { background.spawn(refetch_cameras(generation, client.clone(), tx.clone())); }
                            _ => {}
                        },
                        Ok(_) => {}
                        Err(err) => { let _ = tx.send(Internal::Log(Level::Warn, format!("unparsable device frame: {err}"))); }
                    }
                }
            }
            _ = ping.tick() => {
                ping_socket("events", &mut events).await?;
                ping_socket("devices", &mut devices).await?;
            }
            () = sleep_until(events_seen + DEAD_AFTER) => bail!("events websocket silent for {}s", DEAD_AFTER.as_secs()),
            () = sleep_until(devices_seen + DEAD_AFTER) => bail!("devices websocket silent for {}s", DEAD_AFTER.as_secs()),
            Some(_) = background.join_next() => {}
        }
    }
}

/// Unwraps a websocket read: `Some(text)` for text frames, `None` for control/binary frames.
fn text_frame(
    topic: &str,
    frame: Option<Result<Message, tokio_tungstenite::tungstenite::Error>>,
) -> Result<Option<tokio_tungstenite::tungstenite::Utf8Bytes>> {
    match frame {
        Some(Ok(Message::Text(text))) => Ok(Some(text)),
        Some(Ok(Message::Close(_))) | None => bail!("{topic} websocket closed by console"),
        Some(Ok(_)) => Ok(None),
        Some(Err(err)) => Err(anyhow!("{topic} websocket: {err}")),
    }
}

async fn ping_socket(topic: &str, socket: &mut WsStream) -> Result<()> {
    socket.send(Message::Ping(Bytes::new())).await.map_err(|err| anyhow!("{topic} websocket: {err}"))
}

fn camera_ids(cameras: &[Camera]) -> Vec<String> {
    cameras.iter().map(|c| c.id.clone()).collect()
}

async fn refetch_cameras(generation: u64, client: Arc<Protect>, tx: Tx) {
    match client.cameras().await {
        Ok(cameras) => {
            let ids = camera_ids(&cameras);
            let _ = tx.send(Internal::Cameras { generation, cameras });
            fetch_streams(generation, client, tx, ids).await;
        }
        Err(err) => {
            let _ = tx.send(Internal::Log(Level::Warn, format!("refreshing cameras: {err:#}")));
        }
    }
}

/// Caches every camera's stream URLs (concurrency capped by the client).
async fn fetch_streams(generation: u64, client: Arc<Protect>, tx: Tx, cameras: Vec<String>) {
    let fetches = cameras.into_iter().map(|camera| {
        let (client, tx) = (&client, &tx);
        async move {
            match client.streams(&camera).await {
                Ok(streams) => {
                    let _ = tx.send(Internal::Streams { generation, camera, streams });
                }
                Err(err) => {
                    let _ = tx.send(Internal::Log(Level::Warn, format!("stream URLs for {camera}: {err:#}")));
                }
            }
        }
    });
    futures_util::future::join_all(fetches).await;
}
