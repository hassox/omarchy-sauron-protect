# Agent instructions

Sauron is an Omarchy bar plugin that watches UniFi Protect cameras. A Rust daemon (`daemon/`) talks to the console, and QML files at the repo root draw the eye and panel. The README is the source of truth for user-facing behavior.

## Layout

- `daemon/`: the Rust daemon, installed as `~/.local/bin/sauron`. Edition 2024, binary crate. Modules include `protect.rs` (Protect API client), `private_api.rs` (login and instant-video stream), `tls.rs` (certificate pinning), `daemon.rs` (main loop), `setup.rs` (setup wizard), `config.rs`, `keyring.rs`, `event_log.rs` and `history.rs`.
- `Service.qml`, `Panel.qml`, `EyeIcon.qml`, `SnapshotImage.qml`, `Kinds.js`: the plugin UI.
- `manifest.json`: the Omarchy plugin manifest. Its settings schema and defaults must stay in step with what the daemon and QML read.
- `install.sh`: builds and installs the daemon, then starts setup on first install.
- `docs/`: README images.

## Commands

Run these from `daemon/`. CI runs the same three, so run them before pushing:

```bash
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

Use `cargo fmt` to fix formatting. Commit `Cargo.lock` changes, because CI builds with `--locked`.

## Conventions

- Warnings are errors in CI. Fix clippy findings rather than adding `#[allow]`, and explain any `#[allow]` that is truly needed.
- Use `anyhow` with `.context(...)` for errors. Messages should say what failed and what the user can do about it.
- Match the surrounding code's style, naming and comment density. Comments explain why, not what.
- Add or update unit tests alongside the code (`#[cfg(test)]` modules). Fix a bug with a test that fails without the fix.
- Don't reinvent what a well-established crate already does well, especially for parsing, crypto, TLS, protocols and time handling. Look for a maintained, widely used crate before writing your own. Hand-rolling security-sensitive code is never acceptable.
- Keep dependencies and their features minimal. Disable default features and enable only what's needed. Don't add a crate for something trivial, and prefer one that is small and has few transitive dependencies.
- Update `README.md` when user-visible behavior, setup steps or uninstall steps change. Keep `manifest.json` in step with any new or changed setting.
- The daemon runs on a desktop and talks to a real console. Never hard-code addresses, keys or credentials, and never put real ones in tests or docs.

## Performance

Sauron runs all day in the background on a desktop, so it must be fast and small.

- **Responsive:** never block the async runtime. Use async I/O, put blocking or CPU-heavy work on a blocking thread, and never hold a lock across an `.await`. Notifications and the panel should react right away.
- **Lean on memory:** keep resident memory small and steady. Don't buffer whole streams or images when you can stream them. Bound every queue, cache and history by size. Avoid needless clones and allocations, and borrow or reuse buffers on hot paths such as snapshot refresh and event handling.
- **Quiet when idle:** no busy loops or polling where an event or a long timeout will do. Idle CPU should be close to zero.
- **Small binary:** keep the release profile (thin LTO, `strip`, `panic = "abort"`) and watch for dependencies that bloat it.
- Before merging a change that touches a hot path or adds a dependency, check the effect on memory and binary size, and mention it in the PR.

## Git

- Work on a branch and open a PR against `master`. Don't push to `master` directly.
- Keep commits focused, with a short imperative subject line and a body that says why when it isn't obvious.
- Don't commit anything under `daemon/target/`.

## Security sweep before every commit

Before committing, review the staged diff for security problems. Use the `security-review` skill, or do it by hand against this checklist:

- Credentials: the API key, usernames and passwords go only to the configured console. They must not be logged, put in error messages, or sent anywhere else.
- Network: outbound requests stay inside the pinned TLS boundary. Check client defaults (redirects, proxies, certificate validation) rather than assuming they are safe.
- Installer and setup paths (`install.sh`, `daemon/src/setup.rs`): they only touch what the README documents, and they never run downloaded content unverified.
- Input handling: nothing from the console, the network or the filesystem reaches a shell, a path join or a parser without validation.
- Dependencies: new or upgraded crates are justified and their features are minimal.

Report findings before committing. Fix them or say explicitly why they are acceptable.
