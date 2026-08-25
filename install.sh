#!/usr/bin/env bash
# Install Pastebridge:
#   curl -fsSL https://hapwi.github.io/install/pastebridge.sh | bash
set -euo pipefail

REPO_HTTPS="https://github.com/hapwi/pastebridge"
REPO_GIT="https://github.com/hapwi/pastebridge.git"
RELEASE_TAG="${PASTEBRIDGE_RELEASE:-stable}"
INSTALL_DIR="${PASTEBRIDGE_INSTALL_DIR:-$HOME/.local/bin}"
INSTALLED_BIN=""

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

detect_target() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os-$arch" in
    Linux-x86_64|Linux-amd64) printf '%s\n' "x86_64-unknown-linux-gnu" ;;
    Linux-aarch64|Linux-arm64) printf '%s\n' "aarch64-unknown-linux-gnu" ;;
    Darwin-arm64) printf '%s\n' "aarch64-apple-darwin" ;;
    Darwin-x86_64) printf '%s\n' "x86_64-apple-darwin" ;;
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
  local bin_dir="$1"
  if [[ ":$PATH:" != *":$bin_dir:"* ]]; then
    export PATH="$bin_dir:$PATH"
    local line='export PATH="$HOME/.local/bin:$PATH"'
    local rc
    for rc in "$HOME/.zshrc" "$HOME/.bashrc" "$HOME/.zprofile"; do
      if [[ -f "$rc" ]] && ! grep -Fqs '.local/bin' "$rc"; then
        printf '\n# pastebridge\n%s\n' "$line" >> "$rc"
        say "Added $bin_dir to PATH in $rc"
      fi
    done
  fi
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
  curl -fsSL https://hapwi.github.io/install/pastebridge.sh | bash"
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

place_binary() {
  local src="$1"
  mkdir -p "$INSTALL_DIR"
  install -m 755 "$src" "$INSTALL_DIR/pastebridge"
  INSTALLED_BIN="$INSTALL_DIR/pastebridge"

  local cargo_bin="${CARGO_HOME:-$HOME/.cargo}/bin/pastebridge"
  if [[ -e "$cargo_bin" || -L "$cargo_bin" ]]; then
    ln -sfn "$INSTALLED_BIN" "$cargo_bin"
  fi

  ensure_local_bin_path "$INSTALL_DIR"
}

install_prebuilt() {
  local target archive url tmp expected actual
  target="$(detect_target)" || return 1
  need_cmd tar || fail "tar is required"
  archive="pastebridge-${target}.tar.gz"
  url="${REPO_HTTPS}/releases/download/${RELEASE_TAG}/${archive}"
  tmp="$(mktemp -d)"

  say "Downloading ${target}…"
  if ! curl -fsSL "$url" -o "$tmp/$archive"; then
    rm -rf "$tmp"
    say "No prebuilt binary at $url"
    return 1
  fi

  if curl -fsSL "${REPO_HTTPS}/releases/download/${RELEASE_TAG}/SHA256SUMS" -o "$tmp/SHA256SUMS"; then
    expected="$(awk -v name="$archive" '$2 == name { print $1; exit }' "$tmp/SHA256SUMS")"
    actual="$(file_sha256 "$tmp/$archive")"
    if [[ -z "$expected" ]]; then
      rm -rf "$tmp"
      fail "SHA256SUMS does not list $archive"
    fi
    if [[ "$expected" != "$actual" ]]; then
      rm -rf "$tmp"
      fail "checksum mismatch for $archive"
    fi
  else
    say "No SHA256SUMS published for this release; skipping checksum."
  fi

  tar -xzf "$tmp/$archive" -C "$tmp"
  if [[ ! -f "$tmp/pastebridge" ]]; then
    rm -rf "$tmp"
    fail "archive did not contain pastebridge"
  fi
  place_binary "$tmp/pastebridge"
  rm -rf "$tmp"
}

install_from_source() {
  local root="$1"
  local tmp
  ensure_macos_devtools
  ensure_linux_build
  ensure_rust
  load_cargo_env
  tmp="$(mktemp -d)"
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
  if install_prebuilt; then
    return
  fi
  load_cargo_env
  if need_cmd cargo; then
    say "Falling back to a source build."
    install_from_source ""
    return
  fi
  fail "No prebuilt Pastebridge binary for this computer.
Binaries are published at ${REPO_HTTPS}/releases
Re-run after the stable release exists, or install Rust and set PASTEBRIDGE_FROM_SOURCE=1."
}

main() {
  say "Pastebridge"

  [[ "$(os_name)" == Darwin || "$(os_name)" == Linux ]] \
    || fail "Pastebridge supports macOS and Linux"

  need_cmd curl || fail "curl is required"
  maybe_wl_clipboard

  local root=""
  root="$(resolve_root)"
  install_binary "$root"
  [[ -n "$INSTALLED_BIN" && -x "$INSTALLED_BIN" ]] \
    || fail "pastebridge did not install into $INSTALL_DIR"

  say "Installed $INSTALLED_BIN"
  if ! "$INSTALLED_BIN" install-service; then
    say "Could not enable the login service. Later run: pastebridge install-service"
  fi
  say
  say "Next: pastebridge pair"
  if [[ "$(os_name)" == Darwin ]]; then
    say "Allow clipboard / network permission if macOS asks."
  fi
}

main "$@"
