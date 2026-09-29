# Sauron

The Eye of Sauron for [Omarchy](https://omarchy.org): Barad-dûr in your bar, its eye watching your UniFi Protect cameras.

- **The eye** flickers when a detection you care about (person, vehicle, package, doorbell, …) is happening, and its pupil stays wide until you look.
- **Click it** for a live-refreshing grid of snapshots from every camera, the detection filters, silence controls, and recent sightings.
- **Click a camera** to open its live feed in `mpv`.
- **Notifications** come with a snapshot, and clicking one opens that camera live.

It follows the active Omarchy theme: the tower, fonts, spacing, corner radius, and the foreground, accent, and urgent colours. The eye itself is always fire.

## How it fits together

```
UniFi Protect ──websocket + REST──▶ sauron watch (Rust, tokio) ──JSON lines──▶ Service.qml ──▶ Panel.qml (bar eye + popup)
                                     │                                                         │
                                     ├─▶ D-Bus notifications (snapshot, click → live)          └─▶ click → sauron opens mpv
                                     └─▶ $XDG_RUNTIME_DIR/sauron/snap/*.jpg
```

- `daemon/` is a single-threaded async Rust binary. It holds the two Protect websockets (events and devices) and fetches snapshots only while the popup is open or a detection fires. It also opens live feeds and sends notifications.
- `Service.qml` is a keep-loaded Omarchy shell service. It runs the daemon, keeps its state, and writes your choices to `shell.json`.
- `Panel.qml` is the bar widget and its popup. There is one per monitor, and they all share the service.

The plugin uses Protect's official Integration API with an API key, which needs Protect 5.3 or newer.

## Install

On any Omarchy machine:

```bash
omarchy plugin add https://github.com/hassox/omarchy-sauron
~/.config/omarchy/plugins/sauron/install.sh
```

Or from a clone, which links the clone into the plugins directory:

```bash
git clone https://github.com/hassox/omarchy-sauron
omarchy-sauron/install.sh
```

`install.sh` does the following:

1. Builds the daemon, keeping build output out of the plugins folder (which hot-reloads).
2. Installs the daemon to `~/.local/bin/sauron`.
3. Writes a config template.
4. Puts the eye on the right side of the bar.

It needs Rust. If you don't have it, run `omarchy install dev-env rust`.

After `omarchy plugin update`, re-run `install.sh` so the daemon matches the plugin.

## Configure

1. In Protect, go to **Settings › Control Plane › Integrations** and create an API key.
2. Edit `~/.config/sauron/config.toml` (`sauron config` prints the path):

   ```toml
   host = "192.168.1.1"
   api_key = "…"
   # or keep it in your keyring:
   # api_key_command = "secret-tool lookup service sauron"
   verify_tls = false        # UniFi consoles ship self-signed certificates
   live_quality = "high"     # high | medium | low
   player = ["mpv", "--profile=low-latency", "--untimed", "--no-cache", "--force-window=immediate"]
   ```

3. Check it with `sauron check`.

The eye picks up config changes within a few seconds, with no restart.

## Use

| Where | Action | Result |
|---|---|---|
| Bar eye | left click | Open or close the panel |
| | right click | Live feed of the latest detection |
| | middle click | Silence or wake the eye |
| Panel | `h` `j` `k` `l` / arrows | Move the cursor |
| | Enter / Space | Open a camera, toggle a filter, or pick a silence option |
| | `1`–`9` | Open camera N live |
| | `m` | Mute or unmute alerts from the camera under the cursor (also right-click a tile) |
| | `s` | Silence or wake |
| | `r` | Reconnect to Protect |
| | Tab | Move to the neighbouring bar panel |
| | Esc | Close |

What the eye shows:

| Eye | Meaning |
|---|---|
| Open | Watching |
| Flickering, pupil wide | A detection you care about is happening |
| Pupil wide | You missed sightings; it narrows when you open the panel |
| Half-closed | Silenced |
| Shut, tower dim | Not connected, not configured, or the daemon isn't installed |

### Silence and Do Not Disturb

- **Silence** (from the panel, a middle click, or `s`) stops Sauron's notifications and keeps the eye calm. You can silence for 30 minutes, 1 hour, 8 hours, or until you wake it. Sightings are still recorded in the panel while silenced.
- **Omarchy's Do Not Disturb** is honoured as-is. The notification server files Sauron's notifications straight into history (all of them are sent at normal urgency, so none bypass it), and the eye stops flickering.

### Command line and keybinds

```bash
sauron cameras                 # list cameras
sauron live "front door"       # open a live feed (id, name, or unique prefix)
sauron snapshot driveway       # save a full-quality snapshot
omarchy-shell sauron toggle    # open or close the panel
omarchy-shell sauron latest    # live feed of the latest detection
omarchy-shell sauron silence 60   # silence for an hour (0 = until woken)
omarchy-shell sauron resume
omarchy-shell sauron status
```

To add keybinds, bind any of these in `~/.config/hypr/bindings.lua`.

### Settings

Settings live in the widget's entry in `~/.config/omarchy/shell.json`. You can change them from the panel or with `omarchy bar set`:

```bash
omarchy bar set sauron notify '["person","vehicle","package","ring"]' --json
omarchy bar set sauron desktop false --json          # eye only, no notifications
omarchy bar set sauron snapshotIntervalSec 5 --json  # grid refresh while the panel is open
```

The available `notify` kinds are `person`, `vehicle`, `animal`, `package`, `face`, `licensePlate`, `ring`, `audio` (smoke, CO, siren, glass break, …), and `motion`.

## Develop

A mock Protect console lets you work on the UI without cameras. It serves five cameras with generated snapshots, fires detections on a timer, and takes the API key `mock`:

```bash
cd daemon
cargo run --release --example mock_protect -- --port 7447 --interval 20
curl -X POST 'http://127.0.0.1:7447/mock/trigger?camera=Driveway&kind=person'
```

To drive the eye from it, set `host = "http://127.0.0.1:7447"` and `api_key = "mock"` in `~/.config/sauron/config.toml`. For CLI-only runs you can point `SAURON_CONFIG` at a separate file instead.

The shell caches QML types, so after editing any `.qml` file, run `omarchy restart shell`. Rebuild the daemon with `./install.sh`, which also restarts it.
