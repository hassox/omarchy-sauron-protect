//! `sauron watch`: the long-running bridge between Protect and the shell plugin.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
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
use crate::event_log::{self, CameraLine, EVENT_TYPES, Time, event_kinds};
use crate::notify::{Notification, Notifier};
use crate::private_api::{self, Warmer};
use crate::proto::{CameraOut, Command, Level, Msg, Out, PROTOCOL, Phase, State};
use crate::protect::{self, Camera, DeviceItem, EventItem, Failure, Frame, Protect, WsStream};

const CONFIG_POLL: Duration = Duration::from_secs(3);
const SWEEP_EVERY: Duration = Duration::from_secs(60);
const EVENT_TTL: Duration = Duration::from_secs(30 * 60);
const PING_EVERY: Duration = Duration::from_secs(20);
const DEAD_AFTER: Duration = Duration::from_secs(45);
const BACKOFF_MIN: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Route changes arrive in bursts; wait for this much quiet before acting on them.
const NETWORK_SETTLE: Duration = Duration::from_secs(2);
const AUTH_BACKOFF: Duration = Duration::from_secs(60);
const EVENT_IMAGES_KEPT: usize = 50;
const ENDED_REMEMBERED: usize = 256;

/// Messages from spawned tasks to the main loop. `generation` tags data from a connection
/// session so results from a superseded config are dropped.
enum Internal {
    Stdin(Vec<u8>),
    StdinClosed,
    /// Not online (never `Online`: that comes with its client in `Connected`).
    Status { generation: u64, state: State, message: String },
    /// A session is up on `client`'s address.
    Connected { generation: u64, client: Arc<Protect>, version: String, cameras: Vec<Camera> },
    Cameras { generation: u64, cameras: Vec<Camera> },
    Event { generation: u64, action: String, item: EventItem },
    CameraPatch { generation: u64, item: DeviceItem },
    Disconnected { generation: u64 },
    Snapshot { camera: String, tracked: bool, result: Result<u64, String> },
    /// The system resumed from suspend: every connection is suspect.
    Resumed,
    /// The routing table changed (and then settled): the active address may be wrong now.
    NetworkChanged,
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

/// One connection attempt per config load; replaced on reload and resume.
struct Connection {
    config: Config,
    /// Client for the active address while a session is up.
    client: Option<Arc<Protect>>,
    /// Keeps a private-API session warm for instant live; `None` when instant live is not set up.
    warmer: Option<Arc<Warmer>>,
    /// Tells the session the network changed.
    network: mpsc::UnboundedSender<()>,
    task: JoinHandle<()>,
}

/// Why the connection is being (re)built.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reload {
    Startup,
    /// Config changed or `reload` command: also forget the private-API session.
    Config,
    /// Resumed from suspend: same config, fresh connections.
    Resume,
}

struct Daemon {
    out: Out,
    tx: Tx,
    exe: PathBuf,
    shared: Arc<Shared>,
    config_path: PathBuf,
    config_mtime: Option<SystemTime>,
    generation: u64,
    connection: Option<Connection>,
    /// State, message, Protect version, and the active address when it is a fallback.
    status: (State, Option<String>, Option<String>, Option<String>),
    cameras: HashMap<String, Cam>,
    emitted_cameras: Option<Vec<CamView>>,
    events: HashMap<String, OpenEvent>,
    ended: VecDeque<String>,
    filter: Filter,
    watch: Option<Interval>,
    in_flight: HashSet<String>,
    snapshot_errors: HashMap<String, String>,
    /// Where camera events are appended as JSON Lines (`event_log`); `None` = off.
    event_log: Option<Arc<Path>>,
    event_writer: event_log::Writer,
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
    let log_tx = tx.clone();
    let event_writer = event_log::Writer::new(move |message| {
        let _ = log_tx.send(Internal::Log(Level::Warn, message));
    })
    .context("cannot start the event log writer")?;

    let mut daemon = Daemon {
        out: Out::new(),
        tx: tx.clone(),
        shared: Arc::new(Shared {
            snap_dir,
            events_dir,
            seq: AtomicU64::new(0),
            write_lock: Mutex::new(()),
            notifier: Notifier::new(exe.to_string_lossy().into_owned()),
        }),
        exe,
        config_path: config::config_path(),
        config_mtime: None,
        generation: 0,
        connection: None,
        status: (State::Connecting, None, None, None),
        cameras: HashMap::new(),
        emitted_cameras: None,
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
        event_log: None,
        event_writer,
    };

    emit(&mut daemon.out, &Msg::Hello { version: env!("CARGO_PKG_VERSION"), protocol: PROTOCOL }).await;
    daemon.reload(Reload::Startup).await;
    tokio::spawn(watch_resume(tx.clone()));
    tokio::spawn(watch_network(tx));

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
                    daemon.reload(Reload::Config).await;
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

impl Daemon {
    async fn log(&mut self, level: Level, message: &str) {
        emit(&mut self.out, &Msg::Log { level, message }).await;
    }

    async fn set_status(&mut self, state: State, message: Option<String>, protect: Option<String>, via: Option<String>) {
        let status = (state, message, protect, via);
        if status == self.status {
            return;
        }
        self.status = status;
        let (state, message, protect, via) = &self.status;
        let msg =
            Msg::Status { state: *state, message: message.as_deref(), protect: protect.as_deref(), via: via.as_deref() };
        emit(&mut self.out, &msg).await;
    }

    fn online(&self) -> bool {
        self.status.0 == State::Online
    }

    /// Restarts the connection: re-reading the config, or with the same config after a resume.
    async fn reload(&mut self, why: Reload) {
        if why == Reload::Resume && self.connection.is_none() {
            return;
        }
        let previous = self.connection.take();
        if let Some(connection) = &previous {
            connection.task.abort();
        }
        self.generation += 1;
        self.force_end_all().await;
        if why == Reload::Config {
            private_api::forget_session().await;
        }

        let config = match previous {
            Some(connection) if why == Reload::Resume => Ok(connection.config),
            _ => {
                self.config_mtime = config_mtime(&self.config_path).await;
                config::load(&self.config_path).await.map_err(|err| err.to_string())
            }
        };
        match config {
            Ok(config) => {
                self.set_status(State::Connecting, None, None, None).await;
                self.event_log = config.event_log.as_deref().map(Arc::from);
                let warmer = (!config.username.is_empty()).then(|| Arc::new(Warmer::new(config.username.clone())));
                let (network, network_rx) = mpsc::unbounded_channel();
                let task = tokio::spawn(run_session(self.generation, config.clone(), self.tx.clone(), network_rx));
                self.connection = Some(Connection { config, client: None, warmer, network, task });
            }
            Err(message) => {
                self.set_status(State::Unconfigured, Some(message), None, None).await;
                self.event_log = None;
                self.cameras.clear();
                self.emit_cameras().await;
            }
        }
    }

    async fn handle(&mut self, msg: Internal) {
        match msg {
            Internal::Stdin(line) => self.command(&line).await,
            // The parent shell is gone.
            Internal::StdinClosed => std::process::exit(0),
            Internal::Status { generation, state, message } if generation == self.generation => {
                self.set_status(state, Some(message), None, None).await;
            }
            Internal::Connected { generation, client, version, cameras } if generation == self.generation => {
                let via = client.via().map(str::to_owned);
                if let Some(connection) = &mut self.connection {
                    connection.client = Some(client);
                }
                self.set_cameras(cameras).await;
                self.set_status(State::Online, None, Some(version), via).await;
            }
            Internal::Cameras { generation, cameras } if generation == self.generation => {
                self.set_cameras(cameras).await;
            }
            Internal::Resumed => self.reload(Reload::Resume).await,
            Internal::NetworkChanged => {
                if let Some(connection) = &self.connection {
                    let _ = connection.network.send(());
                }
            }
            Internal::Event { generation, action, item } if generation == self.generation => {
                self.on_event(&action, item).await;
            }
            Internal::CameraPatch { generation, item } if generation == self.generation => {
                self.patch_cameras(item).await;
            }
            Internal::Disconnected { generation } if generation == self.generation => {
                if let Some(connection) = &mut self.connection {
                    connection.client = None;
                }
                self.force_end_all().await;
            }
            Internal::Snapshot { camera, tracked, result } => self.on_snapshot(camera, tracked, result).await,
            Internal::Log(level, message) => self.log(level, &message).await,
            // Stale data from a superseded session.
            Internal::Status { .. }
            | Internal::Connected { .. }
            | Internal::Cameras { .. }
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
            Command::Reload => self.reload(Reload::Config).await,
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
        let Some(client) = self.connection.as_ref().and_then(|c| c.client.clone()) else { return };
        if !self.in_flight.insert(camera.to_owned()) {
            return;
        }
        let (shared, tx, camera) = (self.shared.clone(), self.tx.clone(), camera.to_owned());
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
                self.end_event(&id, end, false).await;
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
        self.log_event(Phase::Start, &id, None, false);
        let open = &self.events[&id];
        if self.filter.matches(&open.camera, &open.kinds) {
            self.event_actions(&id);
        }
        if let Some(end) = item.end {
            self.end_event(&id, end, false).await;
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

    /// Appends a `start` or `end` line to the event log, if one is configured.
    fn log_event(&self, phase: Phase, id: &str, end: Option<i64>, forced: bool) {
        let (Some(path), Some(open)) = (&self.event_log, self.events.get(id)) else { return };
        let line = CameraLine {
            phase,
            id,
            camera: self.camera_name(&open.camera),
            camera_id: &open.camera,
            event_type: &open.event_type,
            kinds: &open.kinds,
            matched: Some(self.filter.matches(&open.camera, &open.kinds)),
            start: Time(open.start),
            end: end.map(Time),
            duration_s: end.map(|end| event_log::duration_s(open.start, end)),
            forced,
        };
        self.event_writer.write(path, &line);
    }

    /// `forced`: ended by us (disconnect, reload, TTL sweep) rather than by Protect.
    async fn end_event(&mut self, id: &str, end: i64, forced: bool) {
        self.emit_event(Phase::End, id, Some(end)).await;
        self.log_event(Phase::End, id, Some(end), forced);
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
            self.end_event(&id, now, true).await;
        }
    }

    async fn sweep(&mut self) {
        let stale: Vec<String> =
            self.events.iter().filter(|(_, e)| e.seen.elapsed() > EVENT_TTL).map(|(id, _)| id.clone()).collect();
        let now = now_ms();
        for id in stale {
            self.end_event(&id, now, true).await;
        }
    }

    /// Desktop notification first, then the snapshot and the notification again with the image;
    /// runs detached so the `event` line is never delayed.
    fn event_actions(&self, id: &str) {
        let (Some(connection), Some(open)) = (&self.connection, self.events.get(id)) else { return };
        let Some(client) = connection.client.clone() else { return };
        let camera_name = self.cameras.get(&open.camera).map_or(open.camera.as_str(), |c| c.info.display_name());
        let job = EventJob {
            event_id: id.to_owned(),
            camera: open.camera.clone(),
            camera_name: camera_name.to_owned(),
            kinds: open.kinds.clone(),
            start: open.start,
            desktop: self.filter.desktop,
        };
        let (shared, warmer, tx) = (self.shared.clone(), connection.warmer.clone(), self.tx.clone());
        tokio::spawn(run_event_job(shared, client, warmer, tx, job));
    }

    // ---- live ----------------------------------------------------------------------------

    async fn live_result(&mut self, camera: &str, ok: bool, message: Option<&str>) {
        emit(&mut self.out, &Msg::Live { camera, ok, message }).await;
    }

    /// Runs `sauron live <camera>` detached: it focuses an open viewer or starts one.
    async fn live(&mut self, camera: String) {
        if self.connection.is_none() {
            return self.live_result(&camera, false, Some("Not set up yet: run sauron setup")).await;
        }
        if !self.cameras.contains_key(&camera) {
            return self.live_result(&camera, false, Some("unknown camera")).await;
        }
        let spawned = tokio::process::Command::new(&self.exe)
            .arg("live")
            .arg(&camera)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn();
        match spawned {
            Ok(mut viewer) => {
                let (tx, name) = (self.tx.clone(), self.camera_name(&camera).to_owned());
                tokio::spawn(async move {
                    match viewer.wait().await {
                        Ok(status) if status.success() => {}
                        Ok(status) => {
                            let _ = tx.send(Internal::Log(Level::Warn, format!("live view of {name}: {status}")));
                        }
                        Err(err) => {
                            let _ = tx.send(Internal::Log(Level::Warn, format!("live view of {name}: {err}")));
                        }
                    }
                });
                self.live_result(&camera, true, None).await;
            }
            Err(err) => {
                let message = format!("cannot start `sauron live`: {err}");
                self.live_result(&camera, false, Some(&message)).await;
            }
        }
    }

    fn camera_name<'a>(&'a self, camera: &'a str) -> &'a str {
        self.cameras.get(camera).map_or(camera, |cam| cam.info.display_name())
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

async fn run_event_job(shared: Arc<Shared>, client: Arc<Protect>, warmer: Option<Arc<Warmer>>, tx: Tx, job: EventJob) {
    let notify = |image: Option<PathBuf>| {
        let (shared, tx, job) = (&shared, &tx, &job);
        async move {
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
    };
    let first = async {
        if job.desktop {
            // An update (kinds grew) keeps the image the earlier notification already shows.
            let previous = shared.events_dir.join(format!("{}.jpg", job.event_id));
            let image = tokio::fs::try_exists(&previous).await.unwrap_or(false).then_some(previous);
            notify(image).await;
        }
    };
    let snapshot = store_snapshot(&shared, &client, &job.camera, Some(&job.event_id), &tx, false);
    let ((), image) = tokio::join!(first, snapshot);
    let image = match image {
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
    if image.is_some() {
        notify(image).await;
    }
    // A click on the notification opens live video: have a private-API session ready for it.
    if let Some(warmer) = warmer
        && let Err(err) = warmer.warm(&client).await
    {
        let _ = tx.send(Internal::Log(Level::Warn, format!("instant live: {err:#}")));
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

// ---- connection session ----------------------------------------------------------------------

/// Connects to the first address of the console that answers, streams both websockets, and
/// reconnects with backoff until aborted. A network change (on `network`) cuts a backoff short,
/// and while online checks whether the active address is still the right one.
async fn run_session(generation: u64, config: Config, tx: Tx, mut network: mpsc::UnboundedReceiver<()>) {
    let config = Arc::new(config);
    let mut backoff = BACKOFF_MIN;
    // The last failure logged in full, so a console that stays away doesn't flood the log.
    let mut logged = String::new();
    loop {
        let (state, message) = match protect::connect(&config).await {
            Ok((client, version)) => {
                let client = Arc::new(client);
                let ended = session_once(generation, &config, &client, version, &tx, &mut network, &mut backoff).await;
                let _ = tx.send(Internal::Disconnected { generation });
                let err = match ended {
                    // A network change: reconnect right away, trying `host` first again.
                    Ok(()) => continue,
                    Err(err) => err,
                };
                let full = format!("{}: {err:#}", client.address());
                if full != logged {
                    let _ = tx.send(Internal::Log(Level::Warn, full.clone()));
                    logged = full;
                }
                if protect::is_auth(&err) {
                    (State::Auth, format!("{err:#}"))
                } else {
                    (State::Offline, protect::failure_message(client.address(), &err))
                }
            }
            Err(err) => {
                let summary = err.summary();
                if summary != logged {
                    for attempt in &err.attempts {
                        let line = format!("{}: {:#}", attempt.address, attempt.error);
                        let _ = tx.send(Internal::Log(Level::Warn, line));
                    }
                    logged = summary;
                }
                let state = match err.failure() {
                    Failure::Auth => State::Auth,
                    Failure::Untrusted => State::Unconfigured,
                    Failure::Offline => State::Offline,
                };
                (state, err.to_string())
            }
        };
        let wait = if state == State::Auth {
            AUTH_BACKOFF
        } else {
            let wait = backoff;
            backoff = (backoff * 2).min(BACKOFF_MAX);
            wait
        };
        let _ = tx.send(Internal::Status { generation, state, message });
        tokio::select! {
            () = sleep(wait) => {}
            Some(()) = network.recv() => {
                while network.try_recv().is_ok() {}
                backoff = BACKOFF_MIN;
            }
        }
    }
}

/// One session on `client`'s address. `Ok(())`: a network change calls for reconnecting now.
async fn session_once(
    generation: u64,
    config: &Arc<Config>,
    client: &Arc<Protect>,
    version: String,
    tx: &Tx,
    network: &mut mpsc::UnboundedReceiver<()>,
    backoff: &mut Duration,
) -> Result<()> {
    let cameras = client.cameras().await?;
    let (mut events, mut devices) = tokio::try_join!(client.subscribe("events"), client.subscribe("devices"))?;

    let mut background = JoinSet::new();
    let mut checks = JoinSet::new();
    let _ = tx.send(Internal::Connected { generation, client: client.clone(), version, cameras });
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
            Some(()) = network.recv(), if checks.is_empty() => {
                checks.spawn(needs_reconnect(config.clone(), client.clone()));
            }
            Some(reconnect) = checks.join_next() => {
                if reconnect.unwrap_or(false) {
                    return Ok(());
                }
            }
            Some(_) = background.join_next() => {}
        }
    }
}

/// After a network change while online: on a fallback, go home as soon as `host` answers as our
/// console (pinned, 3 s); otherwise reconnect only if the active address stopped answering.
async fn needs_reconnect(config: Arc<Config>, client: Arc<Protect>) -> bool {
    if client.via().is_some()
        && let Ok(home) = Protect::new(&config, &config.host)
        && home.alive().await.is_ok()
    {
        return true;
    }
    client.alive().await.is_err()
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

async fn refetch_cameras(generation: u64, client: Arc<Protect>, tx: Tx) {
    match client.cameras().await {
        Ok(cameras) => {
            let _ = tx.send(Internal::Cameras { generation, cameras });
        }
        Err(err) => {
            let _ = tx.send(Internal::Log(Level::Warn, format!("refreshing cameras: {err:#}")));
        }
    }
}

/// Reports `NetworkChanged` each time the routing table changes and then stays quiet for
/// `NETWORK_SETTLE`, from an `ip -o monitor route` child. Without `ip`, logs one warning.
async fn watch_network(tx: Tx) {
    if let Err(err) = watch_network_inner(&tx).await {
        let _ = tx.send(Internal::Log(Level::Warn, format!("cannot watch for network changes: {err:#}")));
    }
}

async fn watch_network_inner(tx: &Tx) -> Result<()> {
    let mut command = tokio::process::Command::new("ip");
    command
        .args(["-o", "monitor", "route"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    // SAFETY: the closure only calls prctl(2), which is async-signal-safe. The watcher must not
    // outlive the daemon, which leaves through `exit` without dropping it.
    unsafe {
        command.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    let mut child = command.spawn().context("cannot run `ip monitor route`")?;
    let stdout = child.stdout.take().context("`ip monitor` has no stdout")?;
    let mut lines = BufReader::new(stdout).lines();
    let mut settle: Option<Instant> = None;
    loop {
        tokio::select! {
            line = lines.next_line() => match line.context("reading `ip monitor`")? {
                Some(_) => settle = Some(Instant::now() + NETWORK_SETTLE),
                None => break,
            },
            () = sleep_until(settle.unwrap_or_else(Instant::now)), if settle.is_some() => {
                settle = None;
                if tx.send(Internal::NetworkChanged).is_err() {
                    return Ok(());
                }
            }
        }
    }
    let status = child.wait().await?;
    bail!("`ip monitor route` exited ({status})")
}

/// Reports `Resumed` after each suspend: logind's `PrepareForSleep(false)` on the system bus.
async fn watch_resume(tx: Tx) {
    if let Err(err) = watch_resume_inner(&tx).await {
        let _ = tx.send(Internal::Log(Level::Warn, format!("cannot watch for resume from suspend: {err:#}")));
    }
}

async fn watch_resume_inner(tx: &Tx) -> Result<()> {
    let bus = zbus::Connection::system().await.context("system bus")?;
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender("org.freedesktop.login1")?
        .path("/org/freedesktop/login1")?
        .interface("org.freedesktop.login1.Manager")?
        .member("PrepareForSleep")?
        .build();
    let mut signals = zbus::MessageStream::for_match_rule(rule, &bus, None).await?;
    while let Some(message) = signals.next().await {
        if let Ok(false) = message?.body().deserialize::<bool>()
            && tx.send(Internal::Resumed).is_err()
        {
            return Ok(());
        }
    }
    bail!("system bus connection closed")
}
