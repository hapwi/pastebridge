#!/usr/bin/env bash
# Download, inspect, and install Pastebridge:
#   curl -fsSLo /tmp/pastebridge-install.sh https://hapwi.github.io/install/pastebridge.sh
#   less /tmp/pastebridge-install.sh
#   bash /tmp/pastebridge-install.sh
set -euo pipefail

REPO_HTTPS="https://github.com/hapwi/pastebridge"
REPO_GIT="https://github.com/hapwi/pastebridge.git"

need_cmd() {
  command -v "$1" >/dev/null 2>&1
}

os_name() {
  uname -s
}

say() {
  printf '%s\n' "$*"
}

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

resolve_root() {
  local src="${BASH_SOURCE[0]:-}"
  if [[ -n "$src" && -f "$src" && "$src" != "bash" && "$src" != "-" ]]; then
    local dir
    dir="$(cd "$(dirname "$src")" && pwd)"
    if [[ -f "$dir/Cargo.toml" ]]; then
      printf '%s\n' "$dir"
      return
    fi
    if [[ -f "$dir/../Cargo.toml" ]]; then
      cd "$dir/.." && pwd
      return
    fi
  fi
  printf '\n'
}

ensure_path_file() {
  local file="$1"
  local line='. "$HOME/.cargo/env"'
  if [[ ! -f "$file" ]]; then
    printf '%s\n' "$line" >> "$file"
    return
  fi
  if grep -Fqs '.cargo/env' "$file"; then
    return
  fi
  printf '\n# rust / pastebridge\n%s\n' "$line" >> "$file"
}

load_cargo_env() {
  export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
  if [[ -f "$HOME/.cargo/env" ]]; then
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
  fi
}

ensure_macos_devtools() {
  if [[ "$(os_name)" != Darwin ]]; then
    return
  fi
  if xcode-select -p >/dev/null 2>&1; then
    return
  fi
  fail "Xcode Command Line Tools are required. Run: xcode-select --install
Then re-run:
  bash /tmp/pastebridge-install.sh"
}

ensure_linux_build() {
  if [[ "$(os_name)" != Linux ]]; then
    return
  fi
  if need_cmd cc || need_cmd gcc || need_cmd clang; then
    return
  fi
  say "A C compiler is needed to build Pastebridge."
  if need_cmd sudo && [[ -r /dev/tty ]]; then
    if need_cmd dnf; then
      sudo -v < /dev/tty
      sudo dnf install -y gcc make pkgconf
    elif need_cmd apt-get; then
      sudo -v < /dev/tty
      sudo apt-get update -qq
      sudo apt-get install -y build-essential pkg-config
    elif need_cmd pacman; then
      sudo -v < /dev/tty
      sudo pacman -S --needed --noconfirm base-devel
    else
      fail "install gcc/make, then re-run this installer"
    fi
  else
    fail "install a C compiler (gcc or clang), then re-run this installer"
  fi
}

ensure_rust() {
  load_cargo_env
  if need_cmd rustc && need_cmd cargo; then
    return
  fi
  fail "Rust and Cargo are required. Install Rust from https://rustup.rs, inspect its instructions, then re-run this installer."
}

maybe_wl_clipboard() {
  if [[ "$(os_name)" != Linux ]]; then
    return
  fi
  if [[ -z "${WAYLAND_DISPLAY:-}" ]]; then
    return
  fi
  if need_cmd wl-copy && need_cmd wl-paste; then
    return
  fi
  say "Wayland detected; installing wl-clipboard…"
  if ! need_cmd sudo || [[ ! -r /dev/tty ]]; then
    say "Could not install wl-clipboard automatically. Install it with:"
    say "  sudo dnf install wl-clipboard"
    say "  sudo apt  install wl-clipboard"
    return
  fi
  sudo -v < /dev/tty || return
  if need_cmd dnf; then
    sudo dnf install -y wl-clipboard || true
  elif need_cmd apt-get; then
    sudo apt-get update -qq
    sudo apt-get install -y wl-clipboard || true
  elif need_cmd pacman; then
    sudo pacman -S --needed --noconfirm wl-clipboard || true
  fi
}

install_binary() {
  local root="$1"
  load_cargo_env
  if [[ -n "$root" && -f "$root/Cargo.toml" ]]; then
    say "Building Pastebridge from this checkout…"
    cargo install --path "$root" --locked --force
  else
    need_cmd git || fail "git is required to install Pastebridge"
    say "Building Pastebridge from $REPO_HTTPS …"
    say "(first install compiles from source; a couple of minutes is normal)"
    cargo install --git "$REPO_GIT" --locked --force
  fi
}

main() {
  say "Pastebridge"
  say "Copy on macOS, paste on Linux — encrypted, local, no account."
  say

  [[ "$(os_name)" == Darwin || "$(os_name)" == Linux ]] \
    || fail "Pastebridge supports macOS and Linux"

  need_cmd curl || fail "curl is required"
  ensure_macos_devtools
  ensure_linux_build
  ensure_rust
  maybe_wl_clipboard

  local root=""
  root="$(resolve_root)"
  install_binary "$root"

  load_cargo_env
  local bin="${CARGO_HOME:-$HOME/.cargo}/bin/pastebridge"
  if [[ ! -x "$bin" ]]; then
    bin="$(command -v pastebridge || true)"
  fi
  [[ -x "$bin" ]] || fail "pastebridge did not install into ~/.cargo/bin"

  if [[ "$(os_name)" == Darwin ]]; then
    mkdir -p "$HOME/.local/bin"
    ln -sfn "$bin" "$HOME/.local/bin/pastebridge" 2>/dev/null || true
  fi

  say
  say "Installed: $bin"
  if ! "$bin" install-service; then
    say "The login service could not be enabled automatically."
    say "Run this after resolving the reported issue: pastebridge install-service"
  fi
  "$bin" doctor || true
  say
  say "Next, pair this computer with the other one:"
  say "  pastebridge pair"
  say
  say "On the other computer run the same installer, then pastebridge pair."
  say "Compare the 8-digit codes. If they match, type y on both."
  say "The login service is already enabled; syncing starts after pairing."
  say
  if [[ "$(os_name)" == Darwin ]]; then
    say "If macOS asks for clipboard or network permission, allow it."
    say "Open a new terminal if the 'pastebridge' command is not found yet."
  fi
}

main "$@"
