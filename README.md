# Pastebridge

**Universal Clipboard for Mac and Linux — without the cloud.**

Copy on your Mac, paste on Linux. Copy on Linux, paste on your Mac. Pastebridge syncs your clipboard between computers over your local network or Tailscale, using the same shortcuts you already know. No account, no cloud upload, no extra window.

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Platforms](https://img.shields.io/badge/platforms-macOS%20%7C%20Linux-lightgrey)](https://github.com/hapwi/pastebridge/releases)
[![Rust](https://img.shields.io/badge/rust-1.80%2B-orange)](https://www.rust-lang.org/)

---

## Why Pastebridge

| | |
|---|---|
| **Feels native** | Works like Apple's Universal Clipboard — `⌘C` / `⌘V` on macOS, `Ctrl+C` / `Ctrl+V` on Linux |
| **Stays local** | Traffic never leaves your LAN or VPN. No server to run, no account to create |
| **Pair once** | Confirm an 8-digit code on both machines. After that, sync runs in the background |
| **Private by design** | TLS 1.3 with pinned certificates, password-manager clipboards blocked, no history on disk |
| **Zero friction** | One-line install, background login service, automatic discovery over mDNS and Tailscale |

---

## Quick start

**1. Install** on macOS or Linux:

```bash
curl -fsSL https://hapwi.github.io/install/pastebridge.sh | bash
```

Downloads a prebuilt binary and enables the user-level login service. Rust is not required.

**2. Pair** both computers:

```bash
pastebridge pair
```

Both screens show an 8-digit code derived from the TLS certificates. If the codes match, type `y` on both. A machine in the middle would show a *different* code — do not confirm if they disagree.

**3. Use it.** Copy text or a screenshot on one machine. Paste on the other with your usual shortcut.

> **Linux (Wayland):** Install `wl-clipboard` — the installer tries to add it automatically.  
> **macOS:** Allow clipboard and local network access if the OS prompts you.

---

## How it works

Pastebridge runs as a background daemon on each machine. When you copy, the daemon sends the clipboard payload to paired peers over encrypted QUIC (UDP). Discovery uses mDNS on the LAN; if [Tailscale](https://tailscale.com/) is installed, running, and logged into the same tailnet on both computers, Pastebridge also discovers desktop peers via `tailscale status` and keeps LAN mDNS as a fallback. Phones and other mobile Tailscale nodes are skipped.

After pairing, the daemon automatically retries saved LAN addresses and current Tailscale addresses. Both computers must be online, Pastebridge and Tailscale (if used) must be running, and **UDP 27419** (sync) and **UDP 27420** (pairing) must be allowed by host firewalls and tailnet ACLs.

Remotely received text and images expire after **3 minutes** by default, but only if the clipboard still contains that exact item. Copying or editing anything newer leaves the clipboard untouched.

---

## Commands

```bash
pastebridge status          # is it running? who is connected?
pastebridge doctor          # clipboard, ports, pairing diagnostics
pastebridge update          # check for a newer release and install
pastebridge list            # paired devices
pastebridge unpair <id>     # forget a device
```

`pastebridge update` checks GitHub for a newer version, shows `0.1.1 → 0.1.2`, and asks `update? [y/N]` before replacing the binary. Pass `-y` to skip the prompt. If the login service is running, it restarts onto the new build.

---

## Security & privacy

- **Encrypted & authenticated** — Every connection uses QUIC with TLS 1.3 and pinned certificates. An unknown machine on the LAN cannot join.
- **Sensitive clipboards blocked** — Password-manager clipboards marked concealed (macOS `org.nspasteboard.ConcealedType`, KDE password hints) are never sent.
- **Safe backends only** — On Linux, Pastebridge refuses to sync when the active clipboard backend cannot expose concealment metadata. Use `wl-clipboard` on Wayland or `xclip` on X11.
- **Nothing on disk** — Clipboard history is not written to disk.
- **Auto-expiry** — Unchanged remotely received clipboards are cleared after 180 seconds by default. Local clipboard items are not expired.
- **Your network, your rules** — No central server. Traffic stays on your LAN or a VPN you already trust.

> Clipboard history managers can retain or restore an item after Pastebridge clears the system clipboard. Pastebridge cannot delete entries from a separate manager's history. Clipboard APIs also do not provide a portable atomic compare-and-clear operation, so Pastebridge compares the exact type and bytes immediately before clearing.

Default ports: **UDP 27419** (sync), **UDP 27420** (pairing), **UDP 5353** (mDNS).

---

## Configuration

`~/.config/pastebridge/config.toml` on Linux  
`~/Library/Application Support/pastebridge/config.toml` on macOS

```toml
bind_address = "0.0.0.0"
port = 27419
pair_port = 27420
max_payload_bytes = 8388608
poll_interval_ms = 400
sync_images = true
# Remote clipboard lifetime in seconds; 0 disables automatic expiry
clipboard_ttl_seconds = 180
# Optional extra addresses (Tailscale, etc.)
static_peers = ["100.64.0.2:27419"]
```

`bind_address = "0.0.0.0"` is required for automatic LAN and Tailscale connectivity. Set a specific address only when you intentionally want to limit Pastebridge to one interface.

Set `clipboard_ttl_seconds = 0` to keep remote clipboard items indefinitely, or another value up to one year. Restart the Pastebridge service after changing configuration.

---

## Uninstall

```bash
pastebridge uninstall-service
rm -f ~/.local/bin/pastebridge
rm -rf ~/.config/pastebridge
# macOS:
# rm -rf ~/Library/Application\ Support/pastebridge
```

---

## License

MIT
