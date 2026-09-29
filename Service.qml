import QtQuick
import Quickshell
import Quickshell.Io
import qs.Commons
import "Kinds.js" as Kinds

// The long-lived half of the plugin. Owns the `sauron watch` daemon, mirrors
// its JSON-lines stream into bindable state, and feeds bar settings, silence
// and panel visibility back to it. One instance serves every monitor's widget.
Item {
  id: root

  property var shell: null
  property var manifest: null

  readonly property string pluginId: "sauron"
  readonly property int protocol: 1
  readonly property string home: Quickshell.env("HOME")
  readonly property string binary: home + "/.local/bin/sauron"
  readonly property string pluginDir: decodeURIComponent(String(Qt.resolvedUrl(".")).replace(/^file:\/\//, "")).replace(/\/$/, "")

  // ------------------------------------------------------------ settings
  // They live in this widget's entry in shell.json. The shell hands plugin
  // services a bar-config snapshot that only refreshes on plugin rescans, so
  // the file itself is the live source.
  readonly property string shellConfigPath: (Quickshell.env("XDG_CONFIG_HOME") || home + "/.config") + "/omarchy/shell.json"
  property var shellConfig: ({})
  readonly property var settings: entryFor(shellConfig.bar)
  readonly property var notifyKinds: settings.notify instanceof Array ? settings.notify : Kinds.defaultNotify
  readonly property var muted: settings.muted instanceof Array ? settings.muted : []
  readonly property bool desktop: settings.desktop !== false
  readonly property int snapshotInterval: Math.max(1, Math.min(60, Number(settings.snapshotIntervalSec) || 2))
  // How many sightings the panel lists; 0 hides the section.
  readonly property int recentLimit: settings.recentSightings === undefined || settings.recentSightings === null
    ? 5 : Math.max(0, Math.min(20, Math.round(Number(settings.recentSightings) || 0)))
  // Epoch ms; -1 silences until resumed, anything in the past is inactive.
  readonly property real silencedUntil: Number(settings.silencedUntil) || 0
  property real now: Date.now()
  readonly property bool silenced: silencedUntil < 0 || silencedUntil > now

  // Omarchy's Do Not Disturb. The notification server already routes our
  // toasts straight to history while it is on; the eye stops pulsing too.
  property bool dnd: false
  readonly property bool quiet: silenced || dnd

  // ------------------------------------------------------------ daemon state
  // starting | missing | unconfigured | connecting | online | offline | auth | outdated
  property string status: "starting"
  property string message: ""
  property string protectVersion: ""
  // The address in use when it isn't the home one (e.g. a Tailscale name); "" at home.
  property string via: ""
  property string daemonVersion: ""
  property var cameras: []
  property var snapshots: ({})   // camera id -> latest snapshot seq
  property var active: ({})      // event id -> matching event still in progress
  property var recent: []        // matching events, newest first
  property int unseen: 0
  property string lastAlertCamera: ""
  property string liveError: ""
  property var watchers: ({})

  readonly property bool online: status === "online"
  readonly property bool watching: Object.keys(watchers).length > 0
  readonly property bool alerting: Object.keys(active).length > 0
  readonly property var activeByCamera: {
    var map = {}
    for (var id in active) {
      var ev = active[id]
      var kinds = map[ev.camera] || []
      for (var i = 0; i < ev.kinds.length; i++) if (kinds.indexOf(ev.kinds[i]) < 0) kinds.push(ev.kinds[i])
      map[ev.camera] = kinds
    }
    return map
  }
  readonly property var alertKinds: {
    var out = []
    for (var cam in activeByCamera) {
      var kinds = activeByCamera[cam]
      for (var i = 0; i < kinds.length; i++) if (out.indexOf(kinds[i]) < 0) out.push(kinds[i])
    }
    return out
  }
  readonly property string alertCamera: {
    for (var cam in activeByCamera) return cam
    return ""
  }
  readonly property int onlineCount: {
    var n = 0
    for (var i = 0; i < cameras.length; i++) if (cameras[i].online) n++
    return n
  }

  property bool _restartPending: false
  property bool _stopping: false
  property int _backoff: 2000

  function entryFor(config) {
    var layout = config && config.layout ? config.layout : null
    if (!layout) return {}
    var sections = ["left", "center", "right"]
    for (var s = 0; s < sections.length; s++) {
      var list = layout[sections[s]] || []
      for (var i = 0; i < list.length; i++) if (list[i] && list[i].id === pluginId) return list[i]
    }
    return {}
  }

  function camera(id) {
    for (var i = 0; i < cameras.length; i++) if (cameras[i].id === id) return cameras[i]
    return null
  }

  function cameraName(id) {
    var c = camera(id)
    return c ? c.name : ""
  }

  // Id, exact name, then unique name prefix, all case-insensitive.
  function findCamera(query) {
    var q = String(query || "").toLowerCase()
    if (q === "") return null
    var prefix = []
    for (var i = 0; i < cameras.length; i++) {
      var c = cameras[i]
      var name = String(c.name).toLowerCase()
      if (c.id.toLowerCase() === q || name === q) return c
      if (name.indexOf(q) === 0) prefix.push(c)
    }
    return prefix.length === 1 ? prefix[0] : null
  }

  function isMuted(id) {
    return muted.indexOf(id) >= 0
  }

  // ------------------------------------------------------------ commands

  function send(obj) {
    if (daemon.running) daemon.write(JSON.stringify(obj) + "\n")
  }

  function pushFilter() {
    var allowed = []
    for (var i = 0; i < cameras.length; i++) if (!isMuted(cameras[i].id)) allowed.push(cameras[i].id)
    var everyone = allowed.length === cameras.length
    send({
      cmd: "filter",
      // Every camera muted: nothing may match, rather than "no allowlist".
      notify: cameras.length > 0 && allowed.length === 0 ? [] : notifyKinds,
      cameras: everyone ? [] : allowed,
      desktop: desktop && !silenced
    })
  }

  function pushWatch() {
    send({ cmd: "watch", on: watching, interval: snapshotInterval })
  }

  function setWatching(key, on) {
    var next = {}
    for (var k in watchers) if (k !== key) next[k] = true
    if (on) next[key] = true
    var was = watching
    watchers = next
    if (watching) unseen = 0
    if (watching !== was) pushWatch()
  }

  function openLive(id) {
    if (!id) return
    liveError = ""
    send({ cmd: "live", camera: id })
  }

  function openLatest() {
    var id = alertCamera || lastAlertCamera || (cameras.length > 0 ? cameras[0].id : "")
    openLive(id)
  }

  function refreshSnapshot(id) {
    send({ cmd: "snapshot", camera: id })
  }

  function reload() {
    send({ cmd: "reload" })
  }

  // Interactive steps run in Omarchy's floating terminal, like its own setup actions.
  function runInTerminal(command) {
    Util.execArgv(["omarchy-launch-floating-terminal-with-presentation", command])
  }

  function setup() {
    runInTerminal(Util.shellQuote(binary) + " setup")
  }

  function install() {
    runInTerminal(Util.shellQuote(pluginDir + "/install.sh"))
  }

  function updateSettings(patch) {
    if (!shell) return
    var next = {}
    for (var k in settings) if (k !== "id") next[k] = settings[k]
    for (var p in patch) next[p] = patch[p]
    shell.updateEntryInline(pluginId, next)
  }

  function toggleKind(kind) {
    var list = notifyKinds.slice()
    var i = list.indexOf(kind)
    if (i >= 0) list.splice(i, 1)
    else list.push(kind)
    updateSettings({ notify: list })
  }

  function toggleMuted(id) {
    var list = []
    for (var i = 0; i < muted.length; i++) if (muted[i] !== id && camera(muted[i])) list.push(muted[i])
    if (!isMuted(id)) list.push(id)
    updateSettings({ muted: list })
  }

  // minutes <= 0 silences until resumed.
  function silence(minutes) {
    now = Date.now()
    var m = Number(minutes) || 0
    updateSettings({ silencedUntil: m > 0 ? now + m * 60000 : -1 })
  }

  function resume() {
    now = Date.now()
    updateSettings({ silencedUntil: 0 })
  }

  function toggleSilence() {
    if (silenced) resume()
    else silence(0)
  }

  function restart() {
    _backoff = 2000
    if (daemon.running) {
      _restartPending = true
      daemon.running = false
    } else {
      probe.running = true
    }
  }

  // ------------------------------------------------------------ daemon stream

  function setState(next, text) {
    status = next
    message = text || ""
  }

  function handleLine(line) {
    var msg
    try { msg = JSON.parse(line) } catch (e) { return }
    switch (msg.t) {
    case "hello":
      daemonVersion = String(msg.version || "")
      if (msg.protocol !== protocol) {
        setState("outdated", "Daemon " + daemonVersion + " speaks protocol " + msg.protocol + "; rebuild it with install.sh")
        return
      }
      pushFilter()
      if (watching) pushWatch()
      break
    case "status":
      protectVersion = msg.protect ? String(msg.protect) : protectVersion
      via = msg.via ? String(msg.via) : ""
      setState(String(msg.state), msg.message)
      if (msg.state === "online") _backoff = 2000
      break
    case "cameras":
      handleCameras(msg.cameras || [])
      break
    case "snapshot":
      var snaps = {}
      for (var k in snapshots) snaps[k] = snapshots[k]
      snaps[msg.camera] = msg.seq
      snapshots = snaps
      break
    case "event":
      handleEvent(msg)
      break
    case "live":
      liveError = msg.ok ? "" : String(msg.message || "Could not open the live feed")
      break
    case "log":
      console.warn("sauron: " + msg.message)
      break
    }
  }

  function handleCameras(list) {
    var snaps = {}
    var known = {}
    for (var i = 0; i < list.length; i++) {
      var c = list[i]
      known[c.id] = true
      snaps[c.id] = Math.max(c.seq || 0, snapshots[c.id] || 0)
    }
    snapshots = snaps
    cameras = list
    // A different console (or none) leaves nothing to show for old sightings.
    var kept = recent.filter(function(ev) { return known[ev.camera] })
    if (kept.length !== recent.length) {
      recent = kept
      if (kept.length === 0) unseen = 0
    }
    pushFilter()
  }

  function handleEvent(msg) {
    var next = {}
    for (var id in active) next[id] = active[id]
    var ev = { id: msg.id, camera: msg.camera, kinds: msg.kinds || [], start: msg.start, end: msg.end || null }

    if (msg.phase === "end") {
      delete next[msg.id]
      active = next
      patchRecent(ev)
      return
    }
    if (!msg.match) {
      // A filter change mid-event: keep tracking what we already showed.
      if (next[msg.id]) { next[msg.id] = ev; active = next; patchRecent(ev) }
      return
    }

    var fresh = !next[msg.id]
    next[msg.id] = ev
    active = next
    if (!fresh) { patchRecent(ev); return }

    lastAlertCamera = ev.camera
    if (!watching && !silenced) unseen++
    recent = Kinds.addSighting(recent, ev, recentLimit)
  }

  function patchRecent(ev) {
    var list = recent.slice()
    for (var i = 0; i < list.length; i++) {
      if (list[i].id !== ev.id) continue
      list[i] = { id: ev.id, camera: ev.camera, kinds: ev.kinds.length > 0 ? ev.kinds : list[i].kinds, start: list[i].start, end: ev.end }
      recent = list
      return
    }
  }

  function daemonExited(code) {
    var forced = active
    active = ({})
    for (var id in forced) patchRecent({ id: id, camera: forced[id].camera, kinds: forced[id].kinds, end: Date.now() })
    if (_stopping) return
    if (_restartPending) {
      _restartPending = false
      probe.running = true
      return
    }
    if (status !== "outdated") setState("offline", "The sauron daemon exited (" + code + "); restarting")
    retryTimer.interval = _backoff
    _backoff = Math.min(_backoff * 2, 30000)
    retryTimer.restart()
  }

  onNotifyKindsChanged: Qt.callLater(pushFilter)
  onMutedChanged: Qt.callLater(pushFilter)
  onDesktopChanged: Qt.callLater(pushFilter)
  onSilencedChanged: Qt.callLater(pushFilter)
  onSnapshotIntervalChanged: if (watching) Qt.callLater(pushWatch)
  onRecentLimitChanged: if (recent.length > recentLimit) recent = recent.slice(0, recentLimit)

  // ------------------------------------------------------------ processes

  Process {
    id: probe
    command: ["test", "-x", root.binary]
    onExited: function(code) {
      if (code === 0) {
        root.setState("starting", "")
        daemon.running = true
        return
      }
      root.setState("missing", root.pluginDir + "/install.sh")
      retryTimer.interval = 5000
      retryTimer.restart()
    }
  }

  Process {
    id: daemon
    command: [root.binary, "watch"]
    stdinEnabled: true
    stdout: SplitParser { onRead: function(line) { root.handleLine(line) } }
    stderr: SplitParser { onRead: function(line) { console.warn("sauron: " + line) } }
    onExited: function(code) { root.daemonExited(code) }
  }

  Timer {
    id: retryTimer
    repeat: false
    onTriggered: probe.running = true
  }

  // Wakes at silence expiry so `silenced` flips back without a settings write.
  Timer {
    running: root.silencedUntil > root.now
    interval: Math.max(1000, Math.min(60000, root.silencedUntil - root.now))
    repeat: true
    onTriggered: root.now = Date.now()
  }

  FileView {
    path: root.shellConfigPath
    watchChanges: true
    printErrors: false
    onFileChanged: reload()
    onLoaded: {
      // A half-written file keeps the previous settings until the next event.
      try { root.shellConfig = JSON.parse(text()) || {} } catch (e) {}
    }
  }

  FileView {
    path: root.home + "/.local/state/omarchy/notifications.json"
    watchChanges: true
    printErrors: false
    onFileChanged: reload()
    onLoaded: {
      try { root.dnd = JSON.parse(text()).dnd === true } catch (e) { root.dnd = false }
    }
    onLoadFailed: root.dnd = false
  }

  ShellIpc {
    target: "sauron"

    function toggle(): void { if (root.shell) root.shell.toggle(root.pluginId, "{}") }
    function live(camera: string): string {
      var c = root.findCamera(camera)
      if (!c) return "unknown camera: " + camera
      root.openLive(c.id)
      return "ok"
    }
    function latest(): string { root.openLatest(); return "ok" }
    function silence(minutes: string): string { root.silence(minutes); return "ok" }
    function resume(): string { root.resume(); return "ok" }
    function restart(): string { root.restart(); return "ok" }
    function setup(): string { root.setup(); return "ok" }
    function status(): string { return root.status + (root.message !== "" ? ": " + root.message : "") }
  }

  Component.onCompleted: probe.running = true
  Component.onDestruction: {
    _stopping = true
    daemon.running = false
  }
}
