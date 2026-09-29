#!/usr/bin/env bash
# Build the sauron daemon, install it to ~/.local/bin, and put the eye on the bar.
#
# Works from a clone anywhere (it links the clone into the Omarchy plugins
# directory) and from the copy `omarchy plugin add` placed there. Re-run it
# after pulling updates so the daemon matches the plugin.
set -euo pipefail

id="sauron"
src="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
plugins="${XDG_CONFIG_HOME:-$HOME/.config}/omarchy/plugins"
bin="$HOME/.local/bin/sauron"

if ! command -v cargo >/dev/null && [[ -f $HOME/.cargo/env ]]; then
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
fi
if ! command -v cargo >/dev/null; then
  echo "sauron builds its daemon with Rust. Install it with: omarchy install dev-env rust" >&2
  exit 1
fi

# Build outside the plugin folder: the shell reloads every plugin on any file
# written inside ~/.config/omarchy/plugins.
target="${XDG_CACHE_HOME:-$HOME/.cache}/sauron/target"
CARGO_TARGET_DIR="$target" cargo build --release --locked --manifest-path "$src/daemon/Cargo.toml"
install -Dm755 "$target/release/sauron" "$bin"
echo "Installed $bin"

if [[ ! -e $plugins/$id ]]; then
  mkdir -p "$plugins"
  ln -s "$src" "$plugins/$id"
  echo "Linked $plugins/$id -> $src"
fi

if command -v omarchy-shell >/dev/null; then
  omarchy-shell shell rescanPlugins >/dev/null 2>&1 || true
  omarchy plugin enable "$id" --section right --after omarchy.agents >/dev/null 2>&1 \
    || omarchy plugin enable "$id" --section right >/dev/null 2>&1 || true
  # Pick up the freshly installed binary if the eye is already running.
  omarchy-shell sauron restart >/dev/null 2>&1 || true
fi

config="${SAURON_CONFIG:-${XDG_CONFIG_HOME:-$HOME/.config}/sauron/config.toml}"
if [[ ! -e $config ]]; then
  if [[ -t 0 && -t 1 ]]; then
    "$bin" setup
  else
    echo "Next: run sauron setup"
  fi
fi
