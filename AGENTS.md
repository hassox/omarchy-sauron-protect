# Agent instructions

Sauron is an Omarchy bar plugin that watches UniFi Protect cameras. A Python daemon (`sauron/`, started by `bin/sauron`) talks to the console, and QML files at the repo root draw the eye and panel. It runs straight from the plugin folder with the system Python: nothing is built or installed. The README is the source of truth for user-facing behavior.

## Layout

- `sauron/`: the asyncio daemon and CLI, Python 3.11+. Modules include `net.py` (verified TLS streams, HTTP over h11, websockets), `protect.py` (Protect API client), `private_api.py` (login and instant-video stream), `daemon.py` (main loop), `onboarding.py` (the panel's connect form), `config.py`, `keyring.py`, `event_log.py`, `history.py` and `live.py`.
- `sauron/_vendor/`: `websockets` and `h11`, copied verbatim from their PyPI wheels. Don't edit them; see its README to update.
- `bin/sauron`: the entry point the shell runs.
- `Service.qml`, `Panel.qml`, `SetupForm.qml`, `EyeIcon.qml`, `SnapshotImage.qml`, `Kinds.js`: the plugin UI.
- `manifest.json`: the Omarchy plugin manifest. Its settings schema and defaults must stay in step with what the daemon and QML read, and its version with `sauron/__init__.py`.
- `tests/`: `unittest` tests. `tools/mock_protect.py` is a fake console that the end-to-end tests run over real TLS.
- `docs/`: README images. `preview.png` is the marketplace card.

## Commands

Run these from the repo root. CI runs the same on Python 3.11 and the latest, so run them before pushing:

```bash
python3 -m compileall -q sauron tools tests bin
python3 -m unittest discover -s tests -t .
```

## Conventions

- Stay compatible with Python 3.11 (no 3.12+ syntax such as backslashes inside f-strings). Use only the standard library and what's vendored.
- Raise exceptions whose messages say what failed and what the user can do about it. Catch broad exceptions only at task boundaries, where the error is reported, and mark them `# noqa: BLE001` with why.
- Match the surrounding code's style, naming and comment density. Comments explain why, not what.
- Add or update tests in `tests/` with the code. Fix a bug with a test that fails without the fix. Anything that talks to the console gets an end-to-end test against the fake console.
- Don't reinvent what a well-established library already does well, especially for parsing, crypto, TLS, protocols and time handling. Look for a maintained, widely used library before writing your own. Hand-rolling security-sensitive code is never acceptable.
- Keep dependencies minimal. Don't add a library for something trivial, and prefer one that is small and has no dependencies of its own.
- Update `README.md` when user-visible behavior, setup steps or uninstall steps change. Keep `manifest.json` in step with any new or changed setting.
- The daemon runs on a desktop and talks to a real console. Never hard-code addresses, keys or credentials, and never put real ones in tests or docs.

## Performance

Sauron runs all day in the background on a desktop, so it must be fast and small.

- **Responsive:** never block the event loop. Use asyncio I/O, put blocking or CPU-heavy work on `asyncio.to_thread`, and don't hold a lock across slow awaits. Notifications and the panel should react right away.
- **Lean on memory:** keep resident memory small and steady. Don't buffer whole streams or images when you can stream them. Bound every queue, cache and history by size. Avoid needless clones and allocations, and borrow or reuse buffers on hot paths such as snapshot refresh and event handling.
- **Quiet when idle:** no busy loops or polling where an event or a long timeout will do. Idle CPU should be close to zero.
- **Few dependencies:** nothing to install. A new library must be pure Python, well established, and vendored with its hash checked (see `sauron/_vendor/README.md`).
- Before merging a change that touches a hot path or adds a dependency, check the effect on memory (the daemon idles around 22 MB RSS) and mention it in the PR.

## Git

- Work on a branch and open a PR against `master`. Don't push to `master` directly.
- Keep commits focused, with a short imperative subject line and a body that says why when it isn't obvious.
- Don't commit `__pycache__/`.

## Security sweep before every commit

Before committing, review the staged diff for security problems. Use the `security-review` skill, or do it by hand against this checklist:

- Credentials: the API key, usernames and passwords go only to the configured console. They must not be logged, put in error messages, or sent anywhere else.
- Network: every connection goes through `net.open_stream`, which checks the certificate before any byte is written. Redirects are never followed.
- Setup paths (`sauron/onboarding.py`, `SetupForm.qml`): they only touch what the README documents, never echo the API key or password back, and never run downloaded content.
- Input handling: nothing from the console, the network or the filesystem reaches a shell, a path join or a parser without validation.
- Dependencies: new or upgraded vendored libraries are justified, pure Python, and hash-checked against PyPI.

Report findings before committing. Fix them or say explicitly why they are acceptable.
