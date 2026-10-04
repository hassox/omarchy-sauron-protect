<p align="center">
  <img src="docs/icon.svg" width="96" alt="">
</p>

<h1 align="center">Sauron</h1>

<p align="center">
  An eye in your <a href="https://omarchy.org">Omarchy</a> bar that watches your UniFi Protect cameras.
</p>

<p align="center">
  <img src="docs/panel.png" width="560" alt="The Sauron panel: detection filters, silence controls, a live grid of camera snapshots with an active detection, and recent sightings">
</p>

Sauron puts Barad-dûr in your status bar. When a camera sees something you care about (a person, a vehicle, a package, the doorbell), the Eye burns, a notification pops up with a snapshot, and one click opens the camera's live feed.

- **See every camera at a glance.** Click the eye for a grid of snapshots that refreshes every couple of seconds while it's open.
- **Get told what matters.** Pick which detections alert you. Silence the eye for half an hour or until you wake it, and it respects Omarchy's Do Not Disturb.
- **Go live in one click.** Click a camera, a sighting, or a notification to open its live feed in `mpv`. With an optional service account the feed starts almost instantly.
- **Fits your desktop.** The tower, panel, fonts, and colours follow your Omarchy theme, and everything works from the keyboard.
- **Stays on your network.** Sauron talks only to your UniFi console. Nothing goes to the cloud.

## Requirements

- **Omarchy** with its built-in bar (`omarchy-shell`).
- **A UniFi OS console running UniFi Protect 5.3 or newer** (Cloud Key, Dream Machine, NVR, …), reachable from your machine.
- **Rust**, to build the Sauron daemon. If you don't have it, run `omarchy install dev-env rust`.

Everything else Sauron uses (`mpv`, `gum`, GNOME Keyring) ships with Omarchy.

## Install

```bash
omarchy plugin add https://github.com/hassox/omarchy-sauron-protect
~/.config/omarchy/plugins/sauron/install.sh
```

The install script:

1. Builds the daemon.
2. Installs it to `~/.local/bin/sauron`.
3. Puts the eye on the right of your bar.
4. On first install, starts the setup wizard.

To update, run `omarchy plugin update sauron`, then run `install.sh` again so the daemon matches the plugin.

### Uninstall

```bash
omarchy plugin remove sauron                 # takes the eye off the bar and deletes the plugin
rm ~/.local/bin/sauron                       # the daemon
rm -r ~/.config/sauron ~/.cache/sauron       # config (holds your API key) and build files
secret-tool clear application sauron        # the service account's password, if you set one up
```

If you turned on the [event log](#event-log-and-history), delete that file too. In UniFi, you can then delete Sauron's API key (**Protect › Settings › Control Plane › Integrations**) and its service account (**Admins & Users**).

## Set up

Run `sauron setup`, or click the eye and then **Set up**. The wizard asks for:

1. **Your console's address.** It suggests your network's gateway, which is usually the console. Sauron then shows the console's certificate and asks you to trust it (see [Away from home](#away-from-home)).
2. **A Protect API key.** Create one in UniFi Protect under **Settings › Control Plane › Integrations › Create API Key**.
3. **Other addresses (optional).** How the console can be reached when you're not at home, such as its Tailscale name.
4. **Instant live video (optional).** See below.

Re-run `sauron setup` any time to change something; your current answers are the defaults. The eye picks up the changes within a few seconds.

### Instant live video (optional)

Without this, live video uses Protect's standard RTSPS stream. That stream can only start at the camera's next keyframe, so a feed takes a few seconds to appear.

The Protect app starts feeds almost instantly with a different stream that needs a UniFi login rather than an API key. To use it, give Sauron a dedicated local account:

1. In UniFi OS, open **Admins & Users** and add a user.
2. Choose **Restrict to local access only**.
3. Give it the **View Only** role for Protect.
4. Leave multi-factor authentication off.

Then enter its username and password in `sauron setup`. The password is stored in your GNOME keyring, not in a file. If that stream ever fails, Sauron falls back to RTSPS.

### Away from home

Sauron needs to reach your console, so away from home it needs a way back to your network:

- **Tailscale.** Run a [subnet router](https://tailscale.com/kb/1019/subnets) at home that advertises your home network, and run `tailscale up --accept-routes` on your laptop. Your console's usual address then works from anywhere. If you reach the console under a different name or IP instead, add it under **Other addresses** in `sauron setup`.
- **UniFi's WireGuard VPN.** In UniFi Network, go to **Settings › VPN › VPN Server**, create a WireGuard server, and import its client file with `nmcli connection import type wireguard file <file>.conf`. (UniFi Teleport also uses WireGuard, but it has no Linux app.)

Without either, the eye shows **Can't reach your console**. It reconnects on its own within a few seconds of your network changing or a VPN connecting, or you can click **Retry**. Detections that happen while you're disconnected aren't recorded, because Protect keeps no history for Sauron to fetch.

**Your credentials only go to your console.** During setup, Sauron records your console's certificate. From then on it won't talk to anything that presents a different certificate: a café router that happens to use the same address as your console gets a failed connection, never your API key or password. If your console's certificate ever changes, re-run `sauron setup` from home.

## Use

| Where | Action | What happens |
|---|---|---|
| Bar eye | Left click | Open or close the panel |
| | Right click | Live feed of the latest detection |
| | Middle click | Silence or wake the eye |
| Panel | Click a camera | Live feed |
| | Right click a camera | Mute or unmute its alerts |
| | Click a sighting | Live feed from that camera |
| | `h` `j` `k` `l` / arrows | Move around |
| | Enter / Space | Activate what's under the cursor |
| | `1`–`9` | Live feed of camera 1–9 |
| | `m` | Mute or unmute the camera under the cursor |
| | `s` | Silence or wake |
| | `r` | Reconnect to Protect |
| | Esc | Close |

<p align="center">
  <img src="docs/eye-states.png" width="600" alt="Eye states: watching, a detection in progress, silenced, and not connected">
</p>

| Eye | Meaning |
|---|---|
| Open | Watching |
| Pupil wide, flames moving | A detection you care about is happening now |
| Pupil wide | You missed sightings; it settles when you open the panel |
| Half closed | Silenced |
| Closed, tower dimmed | Not connected or not set up; open the panel to see why |

### Notifications

<p align="center">
  <img src="docs/notification.png" width="420" alt="A detection notification with a camera snapshot">
</p>

A notification appears as soon as Protect reports a detection you've chosen, and gains a snapshot a moment later. Click it to open that camera live.

- **Silence** (the panel's toggle or silence buttons, a middle click on the eye, or `s`) stops Sauron's notifications for 30 minutes, 1 hour, 8 hours, or until you wake it. Sightings are still listed in the panel.
- **Omarchy's Do Not Disturb** is respected. Sauron's notifications go straight to your notification history instead of popping up, and the eye stays still.

### Live video

<p align="center">
  <img src="docs/live.jpg" width="640" alt="A live camera feed in mpv">
</p>

Live feeds open in `mpv`, one window per camera. Opening a camera that's already open brings its window to the front.

### Keybinds and scripts

```bash
sauron setup                      # set up or change the console, API key, and service account
sauron check                      # test the connection and list cameras
sauron cameras                    # list cameras
sauron live "front door"          # open a live feed (camera id, name, or unique prefix)
sauron snapshot driveway          # save a snapshot to ./Driveway.jpg
sauron log --since 24h            # Protect's event history as JSON Lines (see below)
sauron thumbnail <event-id>       # save Protect's picture of an event
omarchy-shell sauron toggle       # open or close the panel
omarchy-shell sauron latest       # live feed of the latest detection
omarchy-shell sauron silence 60   # silence for 60 minutes (0 = until woken)
omarchy-shell sauron resume       # wake the eye
omarchy-shell sauron status       # connection status
```

Bind any of these in `~/.config/hypr/bindings.lua`.

## Event log and history

Sauron can hand detections to your own scripts: a log to tail, Protect's history to query, and pictures for both.

**Event log.** Set `event_log` in `~/.config/sauron/config.toml` and Sauron appends every camera event to that file as [JSON Lines](https://jsonlines.org): one line when a detection starts and one when it ends.

```toml
event_log = "~/.local/state/sauron/events.jsonl"
```

```json
{"phase":"start","id":"…","camera":"Front Door","camera_id":"…","type":"smartDetectZone","kinds":["person"],"match":true,"start":"2026-09-29T14:02:11.482-05:00","end":null,"duration_s":null}
{"phase":"end","id":"…","camera":"Front Door","camera_id":"…","type":"smartDetectZone","kinds":["person","vehicle"],"match":true,"start":"2026-09-29T14:02:11.482-05:00","end":"2026-09-29T14:02:18.901-05:00","duration_s":7.4}
```

- **What's included.** Every camera event goes in, motion too. `match` says whether it was one you're alerted to.
- **Picking lines.** React to `start` lines while something's happening, and read `end` lines for a record of what happened.
- **Rotation.** Sauron reopens the file for every line, so rotate it however you like (logrotate, a cron job, `mv`) with no signals needed. A handful of cameras produce around 100 KB a day.
- **Gaps.** The log only covers time Sauron was running: not while your machine sleeps or you're away without a VPN. Protect's history has no gaps.

**Protect's history.** `sauron log` prints Protect's own event history in the same shape, oldest first. It needs the [instant live](#instant-live-video-optional) service account, because Protect only shares its history with a login.

```bash
sauron log                         # the last 24 hours
sauron log --since 7d | jq -c 'select(.kinds | index("person"))'
sauron log --since 2026-09-01 --until 2026-09-02
sauron log --all                   # also logins, presence, and device events
```

**Pictures.** `sauron snapshot <camera>` saves what a camera sees now; `sauron thumbnail <event-id>` saves Protect's picture of an event.

For example, to have a local vision model describe each person as they appear:

```bash
ollama pull qwen2.5vl:3b
tail -Fn0 ~/.local/state/sauron/events.jsonl |
  jq --unbuffered -r 'select(.phase == "start" and (.kinds | index("person"))) | [.camera_id, .camera] | @tsv' |
  while IFS=$'\t' read -r id name; do
    img=$(sauron snapshot "$id" "/tmp/sauron-$id.jpg")
    jq -n --arg img "$(base64 -w0 "$img")" \
      '{model: "qwen2.5vl:3b", stream: false, images: [$img],
        prompt: "In one sentence, describe the person in this security camera image."}' |
      curl -s http://127.0.0.1:11434/api/generate -d @- |
      jq -r --arg name "$name" '"\($name): \(.response)"'
  done
```

## Settings

The panel changes these for you. They're stored in the widget's entry in `~/.config/omarchy/shell.json`, and you can also set them with `omarchy bar set`:

| Setting | Default | Meaning |
|---|---|---|
| `notify` | `["person","vehicle","package","ring"]` | Detections that alert you: `person`, `vehicle`, `animal`, `package`, `face`, `licensePlate`, `ring` (doorbell), `audio` (smoke, CO, siren, glass break, …), `motion` |
| `muted` | `[]` | Camera ids that never alert |
| `desktop` | `true` | Show notifications; `false` keeps alerts to the eye and panel |
| `snapshotIntervalSec` | `2` | How often the grid refreshes while the panel is open |
| `recentSightings` | `5` | How many sightings the panel lists; `0` hides them |

```bash
omarchy bar set sauron notify '["person","ring"]' --json
omarchy bar set sauron recentSightings 10 --json
```

Connection settings live in `~/.config/sauron/config.toml`, which `sauron setup` writes:

```toml
host = "192.168.1.1"      # your UniFi console at home
fallback_hosts = []       # other addresses for the same console, e.g. ["unifi.tail1234.ts.net"]
cert_sha256 = "AB:CD:…"   # the console's certificate, recorded by sauron setup
api_key = "…"             # or api_key_command = "secret-tool lookup service sauron"
username = "sauron"       # service account for instant live; its password is in your keyring
event_log = ""            # append camera events here as JSON Lines, e.g. "~/.local/state/sauron/events.jsonl"
live_quality = "high"     # high | medium | low
player = ["mpv", "--profile=low-latency", "--untimed", "--no-cache", "--force-window=immediate"]
```

Sauron hands `mpv` the camera's stream address on stdin, so the access token in it never appears in the process list. A different `player` gets the address as a command-line argument, which other users on the machine can read while it runs.

If your console has a certificate from a public authority (for example on your own domain), leave `cert_sha256` out and Sauron verifies it normally.

## Troubleshooting

- **The eye is closed.** Open the panel. It says what's wrong and offers a button to fix it.
- **Check the connection** with `sauron check`, which prints the Protect version and every camera, or with `omarchy-shell sauron status`.
- **Read the logs** with `journalctl --user -g 'sauron:'`.
- **Live video takes a few seconds to start.** Set up [instant live video](#instant-live-video-optional).
- **Instant live keeps falling back.** Re-run `sauron setup` and re-enter the service account. Sauron pauses its logins for 15 minutes after a rejected password so it can't get the account locked out.
- **"Can't reach your console" at home.** If the panel says the address is *not your console*, the console's certificate has changed; re-run `sauron setup`.

## How it works

```
UniFi Protect ──websockets + REST──▶ sauron watch ──JSON lines──▶ Service.qml ──▶ Panel.qml (bar eye + panel)
                                          │
                                          └──▶ desktop notifications

UniFi Protect ──livestream or RTSPS──▶ sauron live ──▶ mpv
```

- **`sauron watch`** is a small, single-threaded Rust daemon (`daemon/`). It keeps two websockets open to Protect's official Integration API, one for detections and one for camera status, so alerts arrive as soon as Protect reports them. It fetches snapshots only while the panel is open or a detection fires, and sends the notifications.
- **`Service.qml`** runs the daemon inside Omarchy's shell and holds the state.
- **`Panel.qml`** draws the eye and the panel on every monitor.
- **`sauron live`** plays one camera: Protect's livestream when a service account is set up, RTSPS otherwise.

## Development

`daemon/examples/mock_protect.rs` is a fake Protect console. It has five cameras, snapshots, live video, detections on a timer, and a UniFi login. Its API key is `mock`, and its service account is `sauron` with password `mock`.

```bash
cd daemon
cargo run --release --example mock_protect -- --port 7447 --interval 20 --scenes path/to/photos
curl -X POST 'http://127.0.0.1:7447/mock/trigger?camera=Driveway&kind=person'
```

- **Connect to it** with `sauron setup`, using the address `http://127.0.0.1:7447`.
- **Real-looking cameras:** `--scenes` shows `front-door.jpg`, `driveway.jpg`, `backyard.jpg`, `garage.jpg`, and `side-gate.jpg` from that folder instead of test patterns.
- **After editing QML,** run `omarchy restart shell`.
- **After editing the daemon,** run `./install.sh`.

## License and credits

- Sauron is [MIT licensed](LICENSE).
- The camera scenes in the screenshots are rendered from [Poly Haven](https://polyhaven.com) panoramas (CC0).
- Sauron is not affiliated with Ubiquiti.
