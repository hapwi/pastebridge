# Pastebridge

Copy on a Mac, paste on Linux. Copy on Linux, paste on a Mac.

No account, no cloud, no extra window. Pair the two computers once. After that it behaves like Apple’s Universal Clipboard: the normal copy and paste shortcuts, on both operating systems.

## Install

On macOS or Linux:

```bash
curl -fsSLo /tmp/pastebridge-install.sh https://hapwi.github.io/install/pastebridge.sh
less /tmp/pastebridge-install.sh
bash /tmp/pastebridge-install.sh
```

Inspect the installer before running it. Rust and Cargo must already be installed.
The installer builds Pastebridge and enables its user-level login service.

Then pair both computers:

```bash
pastebridge pair
```

Compare the 8-digit codes. If they match, type `y` on both.

After pairing, the already-installed login service reloads the peer and begins
syncing automatically.

Linux users on Wayland should have `wl-clipboard` (the installer tries to add it). On macOS, allow clipboard / local network permission if the OS asks.

## Pair

1. On computer A: `pastebridge pair`
2. On computer B: `pastebridge pair`
3. Both screens show an 8-digit code. If the codes match, type `y` on both.

The code is computed from the TLS certificates. A machine in the middle would show a *different* code, so do not confirm if they disagree.

If Tailscale is installed, running, and logged into the same tailnet on both
computers, use the normal command on both:

```bash
pastebridge pair
```

Pastebridge reads `tailscale status --json` and `tailscale ip -4` directly,
discovers online tailnet peers, and keeps LAN mDNS as a fallback. It does not
change Tailscale settings or bypass tailnet policy. If automatic discovery is
unavailable, one side can still use
`pastebridge pair --connect 100.x.y.z:27420`.

After pairing, the daemon automatically retries saved LAN addresses and current
Tailscale addresses. Both computers still need to be online, Pastebridge and
Tailscale must be running, and UDP 27419/27420 must be allowed by host firewalls
and the tailnet ACL.

## Use

Copy text or a screenshot on one computer. Paste on the other with the usual shortcut.
Remotely received text and images are cleared after 3 minutes by default, but
only if the clipboard still contains that exact item. Copying or editing
anything newer leaves the clipboard untouched.

```
pastebridge status          # is it running? who is connected?
pastebridge doctor          # clipboard, ports, pairing
pastebridge list            # paired devices
pastebridge unpair <id>     # forget a device
```

## How it stays private

- After pairing, every connection is QUIC (TLS 1.3) with **pinned certificates**. An unknown machine on the LAN cannot join.
- Password-manager clipboards marked concealed (macOS `org.nspasteboard.ConcealedType`, KDE password hints) are not sent.
- On Linux, Pastebridge refuses to sync when the active clipboard backend cannot expose concealment metadata. Install `wl-clipboard` on Wayland or `xclip` on X11.
- Clipboard history is not written to disk.
- An unchanged remotely received clipboard is cleared after 180 seconds by default. Local clipboard items are not expired.
- There is no server to run and no account to create. Traffic stays on your LAN, or on a VPN you already trust such as Tailscale.

Clipboard history managers can retain or restore an item after Pastebridge
clears the system clipboard. Pastebridge cannot delete entries from a separate
clipboard manager's history. Clipboard APIs also do not provide a portable
atomic compare-and-clear operation, so Pastebridge compares the exact type and
bytes immediately before clearing.

Default ports: **UDP 27419** (sync) and **UDP 27420** (pairing). mDNS uses UDP 5353.

## Config

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

`bind_address = "0.0.0.0"` is required for automatic LAN and Tailscale
connectivity. Set a specific address only when you intentionally want to limit
Pastebridge to one interface.

Set `clipboard_ttl_seconds = 0` to keep remote clipboard items indefinitely, or
set another number of seconds up to one year. Restart the Pastebridge service
after changing the configuration.

## Uninstall

```bash
pastebridge uninstall-service
cargo uninstall pastebridge
rm -rf ~/.config/pastebridge
# macOS:
# rm -rf ~/Library/Application\ Support/pastebridge
```

## License

MIT
