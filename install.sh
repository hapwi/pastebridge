#!/usr/bin/env bash
# Install Pastebridge:
#   curl -fsSL https://raw.githubusercontent.com/hapwi/pastebridge/main/install.sh | bash
set -euo pipefail

REPO_HTTPS="https://github.com/hapwi/pastebridge"
REPO_GIT="https://github.com/hapwi/pastebridge.git"
RELEASE_TAG="${PASTEBRIDGE_RELEASE:-latest}"
RELEASE_BASE="${PASTEBRIDGE_RELEASE_BASE_URL:-$REPO_HTTPS/releases/download}"
INSTALL_DIR="${PASTEBRIDGE_INSTALL_DIR:-$HOME/.local/bin}"
INSTALLED_BIN=""
WORK_DIR=""
ACTIVATION_DIR=""
ACTIVATED=0
SMOKE_PID=""

cleanup() {
  local result=$?
  trap - EXIT
  if [[ -n "$SMOKE_PID" ]]; then
    kill "$SMOKE_PID" 2>/dev/null || true
    wait "$SMOKE_PID" 2>/dev/null || true
  fi
  if [[ "$ACTIVATED" == 1 ]]; then
    if [[ -f "$ACTIVATION_DIR/previous" ]]; then
      if mv -f "$ACTIVATION_DIR/previous" "$INSTALLED_BIN"; then
        say "Restored the previous Pastebridge executable."
        "$INSTALLED_BIN" install-service </dev/null || say "Run $INSTALLED_BIN install-service to restore the service."
      else
        say "Could not restore the previous executable. It remains at $ACTIVATION_DIR/previous" >&2
        ACTIVATION_DIR=""
      fi
    else
      "$INSTALLED_BIN" uninstall-service </dev/null || true
      rm -f "$INSTALLED_BIN"
    fi
  fi
  [[ -z "$WORK_DIR" ]] || rm -rf "$WORK_DIR"
  [[ -z "$ACTIVATION_DIR" ]] || rm -rf "$ACTIVATION_DIR"
  exit "$result"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

fetch() {
  curl -fsSL --connect-timeout 10 --max-time 120 --retry 2 --retry-delay 1 "$1" -o "$2"
}

resolve_release() {
  if [[ "$RELEASE_TAG" == latest || "$RELEASE_TAG" == stable ]]; then
    local url
    url="$(curl -fsSLI --connect-timeout 10 --max-time 30 --retry 2 -o /dev/null -w '%{url_effective}' "$REPO_HTTPS/releases/latest")" \
      || fail "could not resolve the latest release"
    RELEASE_TAG="${url##*/}"
  fi
  [[ "$RELEASE_TAG" == v* ]] || RELEASE_TAG="v$RELEASE_TAG"
  [[ "$RELEASE_TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] \
    || fail "PASTEBRIDGE_RELEASE must be latest or an exact version such as v0.1.6"
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1
}

os_name() {
  uname -s
}

say() {
  printf '%s\n' "$*"
}

step() {
  printf '  → %s\n' "$*"
}

done_step() {
  printf '  ✓ %s\n' "$*"
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

detect_target() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os-$arch" in
    Linux-x86_64|Linux-amd64) printf '%s\n' "x86_64-unknown-linux-gnu" ;;
    Linux-aarch64|Linux-arm64) printf '%s\n' "aarch64-unknown-linux-gnu" ;;
    Darwin-arm64) printf '%s\n' "aarch64-apple-darwin" ;;
    *) return 1 ;;
  esac
}

file_sha256() {
  if need_cmd sha256sum; then
    sha256sum "$1" | awk '{print $1}'
  elif need_cmd shasum; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    fail "sha256sum or shasum is required to verify the download"
  fi
}

ensure_local_bin_path() {
  local bin_dir="$1" quoted line rc
  if [[ ":$PATH:" == *":$bin_dir:"* ]]; then
    return
  fi
  quoted="$(printf '%s' "$bin_dir" | sed 's/[\\$`"]/\\&/g')"
  line="$(printf 'export PATH="%s:$PATH"' "$quoted")"
  local profiles=()
  local shell_name="${SHELL:-/bin/sh}"
  case "${shell_name##*/}" in
    zsh) profiles=("${ZDOTDIR:-$HOME}/.zshrc" "${ZDOTDIR:-$HOME}/.zprofile") ;;
    bash)
      profiles=("$HOME/.bashrc")
      if [[ -f "$HOME/.bash_profile" ]]; then profiles+=("$HOME/.bash_profile")
      elif [[ -f "$HOME/.bash_login" ]]; then profiles+=("$HOME/.bash_login")
      else profiles+=("$HOME/.profile"); fi ;;
    *) profiles=("$HOME/.profile") ;;
  esac
  for rc in "${profiles[@]}"; do
    mkdir -p "$(dirname "$rc")"
    if ! grep -Fqx "$line" "$rc" 2>/dev/null; then
      printf '\n# pastebridge\n%s\n' "$line" >> "$rc"
    fi
  done
  export PATH="$bin_dir:$PATH"
  done_step "Terminal command configured"
  say "  Open a new terminal to use the pastebridge command. Setup continues here."
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
  fail "Xcode Command Line Tools are required to build from source. Run: xcode-select --install
Then re-run:
  curl -fsSL https://raw.githubusercontent.com/hapwi/pastebridge/main/install.sh | bash"
}

ensure_linux_build() {
  if [[ "$(os_name)" != Linux ]]; then
    return
  fi
  if need_cmd cc || need_cmd gcc || need_cmd clang; then
    return
  fi
  say "A C compiler is needed to build Pastebridge from source."
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
  fail "Rust and Cargo are required to build from source. Install Rust from https://rustup.rs, then re-run this installer."
}

ensure_linux_clipboard() {
  [[ "$(os_name)" == Linux ]] || return 0
  local package="" socket
  if [[ -n "${WAYLAND_DISPLAY:-}" ]]; then
    package="wl-clipboard"
  elif [[ -n "${XDG_RUNTIME_DIR:-}" ]]; then
    for socket in "$XDG_RUNTIME_DIR"/wayland-*; do
      if [[ -S "$socket" ]]; then package="wl-clipboard"; break; fi
    done
  fi
  if [[ -z "$package" && -n "${DISPLAY:-}" ]]; then package="xclip"; fi
  [[ -n "$package" ]] || return 0
  if [[ "$package" == wl-clipboard ]] && need_cmd wl-copy && need_cmd wl-paste; then return 0; fi
  if [[ "$package" == xclip ]] && need_cmd xclip; then return 0; fi
  say "Installing $package for your Linux clipboard..."
  if ! need_cmd sudo || ! ( : </dev/tty ) 2>/dev/null; then
    fail "install $package with your package manager, then run this installer again"
  fi
  sudo -v </dev/tty
  if need_cmd dnf; then
    sudo dnf install -y "$package"
  elif need_cmd apt-get; then
    sudo apt-get update -qq
    sudo apt-get install -y "$package"
  elif need_cmd pacman; then
    sudo pacman -S --needed --noconfirm "$package"
  else
    fail "install $package with your package manager, then run this installer again"
  fi
  if [[ "$package" == wl-clipboard ]]; then
    need_cmd wl-copy && need_cmd wl-paste || fail "wl-clipboard did not install"
  else
    need_cmd xclip || fail "xclip did not install"
  fi
}

place_binary() {
  local src="$1" version attempt
  if [[ "$(os_name)" == Darwin ]]; then
    xattr -d com.apple.quarantine "$src" >/dev/null 2>&1 || true
  fi
  "$src" --version > "$WORK_DIR/version" &
  SMOKE_PID=$!
  for ((attempt=0; attempt<100; attempt++)); do
    kill -0 "$SMOKE_PID" 2>/dev/null || break
    sleep 0.1
  done
  if kill -0 "$SMOKE_PID" 2>/dev/null; then
    fail "the downloaded executable timed out during its version check"
  fi
  wait "$SMOKE_PID" || fail "the downloaded executable does not run"
  SMOKE_PID=""
  version="$(cat "$WORK_DIR/version")"
  [[ "$version" =~ ^pastebridge\ [0-9]+\.[0-9]+\.[0-9]+$ ]] \
    || fail "the downloaded executable did not report a Pastebridge version"
  if [[ "$RELEASE_TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ && "${PASTEBRIDGE_FROM_SOURCE:-}" != 1 ]]; then
    [[ "$version" == "pastebridge ${RELEASE_TAG#v}" ]] || fail "downloaded version does not match $RELEASE_TAG"
  fi
  mkdir -p "$INSTALL_DIR"
  INSTALL_DIR="$(cd "$INSTALL_DIR" && pwd)"
  INSTALLED_BIN="$INSTALL_DIR/pastebridge"
  ACTIVATION_DIR="$(mktemp -d "$INSTALL_DIR/.pastebridge-install.XXXXXX")"
  install -m 755 "$src" "$ACTIVATION_DIR/pastebridge"
  if [[ -e "$INSTALLED_BIN" ]]; then
    cp -p "$INSTALLED_BIN" "$ACTIVATION_DIR/previous"
  fi
  mv -f "$ACTIVATION_DIR/pastebridge" "$INSTALLED_BIN"
  ACTIVATED=1
}

install_prebuilt() {
  local target archive expected actual
  target="$(detect_target)" || fail "no prebuilt Pastebridge binary for this OS and processor"
  need_cmd tar || fail "tar is required"
  resolve_release
  archive="pastebridge-${target}.tar.gz"
  WORK_DIR="$(mktemp -d)"
  step "Download Pastebridge ${RELEASE_TAG#v} · ${target}"
  fetch "$RELEASE_BASE/$RELEASE_TAG/SHA256SUMS" "$WORK_DIR/SHA256SUMS" \
    || fail "could not download checksums; refusing an unverified installation"
  expected="$(awk -v name="$archive" '$2 == name || $2 == "*" name { print $1; exit }' "$WORK_DIR/SHA256SUMS")"
  [[ "$expected" =~ ^[[:xdigit:]]{64}$ ]] || fail "SHA256SUMS does not contain a valid checksum for $archive"
  fetch "$RELEASE_BASE/$RELEASE_TAG/$archive" "$WORK_DIR/$archive" \
    || fail "could not download $archive; your existing installation has not changed"
  actual="$(file_sha256 "$WORK_DIR/$archive")"
  [[ "$expected" == "$actual" ]] || fail "checksum mismatch for $archive"
  done_step "Download and checksum verified"
  tar -xzf "$WORK_DIR/$archive" -C "$WORK_DIR" pastebridge
  [[ -f "$WORK_DIR/pastebridge" && ! -L "$WORK_DIR/pastebridge" ]] \
    || fail "archive did not contain a regular pastebridge executable"
  place_binary "$WORK_DIR/pastebridge"
}

install_from_source() {
  local root="$1"
  local tmp
  ensure_macos_devtools
  ensure_linux_build
  ensure_rust
  load_cargo_env
  tmp="$(mktemp -d)"
  WORK_DIR="$tmp"
  if [[ -n "$root" && -f "$root/Cargo.toml" ]]; then
    say "Building Pastebridge from this checkout…"
    cargo install --path "$root" --locked --force --root "$tmp"
  else
    need_cmd git || fail "git is required to install Pastebridge from source"
    say "Building Pastebridge from $REPO_HTTPS …"
    say "(compiles from source; a couple of minutes is normal)"
    cargo install --git "$REPO_GIT" --locked --force --root "$tmp"
  fi
  place_binary "$tmp/bin/pastebridge"
  rm -rf "$tmp"
}

install_binary() {
  local root="$1"
  if [[ -n "$root" && -f "$root/Cargo.toml" ]]; then
    install_from_source "$root"
    return
  fi
  if [[ "${PASTEBRIDGE_FROM_SOURCE:-}" == "1" ]]; then
    install_from_source ""
    return
  fi
  install_prebuilt

}

main() {
  say ""
  if [[ -t 1 && -z "${NO_COLOR+x}" && "${TERM:-}" != dumb ]]; then
    printf '  \033[1;36mPastebridge\033[0m  \033[2m/ Setup\033[0m\n\n'
  else
    printf '  Pastebridge / Setup\n\n'
  fi

  [[ "$(os_name)" == Darwin || "$(os_name)" == Linux ]] \
    || fail "Pastebridge supports macOS and Linux"
  if [[ "$(os_name)" == Darwin && "$(uname -m)" != arm64 ]]; then
    fail "Pastebridge requires Apple Silicon on macOS"
  fi

  need_cmd curl || fail "curl is required"
  ensure_linux_clipboard

  local root=""
  root="$(resolve_root)"
  install_binary "$root"
  [[ -n "$INSTALLED_BIN" && -x "$INSTALLED_BIN" ]] \
    || fail "pastebridge did not install into $INSTALL_DIR"

  step "Start the clipboard service"
  if ! "$INSTALLED_BIN" install-service; then
    fail "Pastebridge could not start its login service; check the diagnostics above"
  fi
  ACTIVATED=0
  ensure_local_bin_path "$INSTALL_DIR"
  local cargo_bin="${CARGO_HOME:-$HOME/.cargo}/bin/pastebridge"
  if [[ ( -e "$cargo_bin" || -L "$cargo_bin" ) && ! "$cargo_bin" -ef "$INSTALLED_BIN" ]]; then
    ln -sfn "$INSTALLED_BIN" "$cargo_bin"
  fi
  done_step "Pastebridge is installed and ready"
  say "  Starts automatically when you log in to your desktop."
  say ""
  if [[ "$(os_name)" == Darwin ]]; then
    say "  Allow Local Network access if macOS asks."
  fi
  local paired
  if ! paired="$("$INSTALLED_BIN" list --quiet 2>/dev/null)"; then
    paired="$("$INSTALLED_BIN" list)" || fail "could not read paired devices"
    [[ "$paired" != "no paired devices" ]] || paired=""
  fi
  if [[ -z "$paired" && "${PASTEBRIDGE_NO_PAIR:-}" != 1 ]]; then
    say "  Next · pair your computers"
    say "  Run this installer on your other computer too. Compare the code on both screens."
    if ( : </dev/tty ) 2>/dev/null; then
      "$INSTALLED_BIN" pair </dev/tty
    else
      say "Run $INSTALLED_BIN pair in an interactive terminal on both computers."
    fi
  elif [[ -z "$paired" ]]; then
    say "  Next: $INSTALLED_BIN pair"
  else
    done_step "Your saved computers are preserved"
    "$INSTALLED_BIN" status
    say "  Reinstalled another computer? Run pastebridge pair on both to pair it again."
  fi
  say ""
  say "  Open dashboard   pastebridge"
  say "  Show computers   pastebridge devices"
  say "  Forget computer  pastebridge unpair"
  say "  Check updates    pastebridge update"
  say ""
}

main "$@"
