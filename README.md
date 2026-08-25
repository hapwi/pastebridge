# Pastebridge

Copy on a Mac, paste on Linux. Copy on Linux, paste on a Mac.

No account, no cloud, no extra window. Pair the two computers once. After that it behaves like Apple’s Universal Clipboard: the normal copy and paste shortcuts, on both operating systems.

## Install

On macOS or Linux:

```bash
curl -fsSL https://hapwi.github.io/install/pastebridge.sh | bash
```

That installs Rust if needed, builds Pastebridge, and sets it to start when you log in.

Then pair both computers:

```bash
pastebridge pair
```

Compare the 8-digit codes. If they match, type `y` on both.

Linux users on Wayland should have `wl-clipboard` (the installer tries to add it). On macOS, allow clipboard / local network permission if the OS asks.

## Pair

1. On computer A: `pastebridge pair`
2. On computer B: `pastebridge pair`
3. Both screens show an 8-digit code. If the codes match, type `y` on both.

The code is computed from the TLS certificates. A machine in the middle would show a *different* code, so do not confirm if they disagree.

If the computers are not on the same LAN (one is on Tailscale, you are in another building, etc.):

```bash
# on the Linux box, note the address it prints, then on the Mac:
pastebridge pair --connect 100.x.y.z:27420
```

Only one side should pass `--connect`. The other side just runs `pastebridge pair`.

## Use

Copy text or a screenshot on one computer. Paste on the other with the usual shortcut.

```
pastebridge status          # is it running? who is connected?
pastebridge doctor          # clipboard, ports, pairing
pastebridge list            # paired devices
pastebridge unpair <id>     # forget a device
```

## How it stays private

- After pairing, every connection is QUIC (TLS 1.3) with **pinned certificates**. An unknown machine on the LAN cannot join.
- Password-manager clipboards marked concealed (macOS `org.nspasteboard.ConcealedType`, KDE password hints) are not sent.
- Clipboard history is not written to disk.
- There is no server to run and no account to create. Traffic stays on your LAN, or on a VPN you already trust such as Tailscale.

Default ports: **UDP 27419** (sync) and **UDP 27420** (pairing). mDNS uses UDP 5353.

## Config

`~/.config/pastebridge/config.toml` on Linux  
`~/Library/Application Support/pastebridge/config.toml` on macOS

```toml
port = 27419
pair_port = 27420
max_payload_bytes = 8388608
poll_interval_ms = 400
sync_images = true
# Optional extra addresses (Tailscale, etc.)
static_peers = ["100.64.0.2:27419"]
```

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
