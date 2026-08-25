use anyhow::{Context, Result};
use arboard::{Clipboard as Arboard, ImageData};
use image::{ImageBuffer, ImageFormat, ImageReader, RgbaImage};
use std::borrow::Cow;
use std::fs;
use std::io::Cursor;
use std::io::Read;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};

use crate::config::MAX_PAYLOAD_BYTES;
use crate::crypto::clip_hash;

const MAX_IMAGE_DIMENSION: usize = 8192;
const MAX_IMAGE_DECODED_BYTES: usize = 64 * 1024 * 1024;
const IMAGE_PASTE_MIMES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/jpg",
    "image/gif",
    "image/webp",
    "image/bmp",
    "image/tiff",
];

#[derive(Debug, Clone)]
pub struct Clip {
    pub mime: String,
    pub bytes: Vec<u8>,
    pub concealed: bool,
}

impl Clip {
    pub fn hash(&self) -> String {
        clip_hash(&self.mime, &self.bytes)
    }
}

pub struct Clipboard {
    inner: Option<Arboard>,
    last_hash: Option<String>,
    ignore_hash: Option<String>,
    file_identity: Option<String>,
    file_clip: Option<Clip>,
    pub backend: String,
}

impl Clipboard {
    pub fn open() -> Result<Self> {
        apply_wayland_env();
        let inner = Arboard::new().ok();
        let backend = detect_backend(inner.is_some());
        Ok(Self {
            inner,
            last_hash: None,
            ignore_hash: None,
            file_identity: None,
            file_clip: None,
            backend,
        })
    }

    pub fn poll(&mut self) -> Option<Clip> {
        let clip = self.read()?;
        if clip.bytes.is_empty() {
            return None;
        }
        let hash = clip.hash();
        if self.last_hash.as_deref() == Some(&hash) {
            return None;
        }
        if self.ignore_hash.as_deref() == Some(&hash) {
            self.last_hash = Some(hash);
            return None;
        }
        self.last_hash = Some(hash);
        Some(clip)
    }

    pub fn set(&mut self, clip: &Clip) -> Result<()> {
        self.clear_file_cache();
        self.ignore_hash = Some(clip.hash());
        self.last_hash = self.ignore_hash.clone();
        match clip.mime.as_str() {
            "image/png" => self.set_image(&clip.bytes),
            _ => self.set_text(String::from_utf8_lossy(&clip.bytes).as_ref()),
        }
    }

    /// Clears the clipboard only when its current supported content is exactly `expected`.
    ///
    /// Clipboard protocols do not provide a portable compare-and-clear operation, so this
    /// performs the comparison immediately before the backend clear.
    pub fn clear_if_matches(&mut self, expected: &Clip) -> Result<bool> {
        let Some(current) = self.read() else {
            return Ok(false);
        };
        if !same_content(&current, expected) {
            return Ok(false);
        }
        self.clear()?;
        Ok(true)
    }

    pub fn read(&mut self) -> Option<Clip> {
        let concealed = is_concealed();

        // A photo file from Finder/Nautilus is a file URI; send it as pixels.
        // Other files (PDFs, zips) are not synced — fall through so a browser
        // "Copy Image" that also offers a file URI / URL can still send pixels.
        if let Some(paths) = self.read_local_file_paths() {
            if let Some(clip) = self.image_clip_from_paths(paths, concealed) {
                return Some(clip);
            }
        }
        self.clear_file_cache();

        // Copy image / screenshot / "Copy Image" in a browser: pixels beat
        // companion HTML and URL metadata.
        if let Some(png) = self.read_image_png() {
            if !png.is_empty() {
                return Some(Clip {
                    mime: "image/png".into(),
                    bytes: png,
                    concealed,
                });
            }
        }

        if let Some(text) = self.read_text() {
            let text = normalize_clipboard_text(&text);
            if !text.is_empty() {
                return Some(Clip {
                    mime: "text/plain".into(),
                    bytes: text.into_bytes(),
                    concealed,
                });
            }
        }

        None
    }

    fn read_text(&mut self) -> Option<String> {
        if let Some(text) = wl_paste_text()
            .or_else(xclip_paste_text)
            .or_else(pb_paste_text)
        {
            return Some(text);
        }
        if let Some(cb) = self.inner.as_mut() {
            if let Ok(text) = cb.get_text() {
                if !text.is_empty() {
                    return Some(text);
                }
            }
        }
        None
    }

    fn read_image_png(&mut self) -> Option<Vec<u8>> {
        if let Some(png) = wl_paste_image().or_else(xclip_paste_image) {
            return Some(png);
        }
        #[cfg(target_os = "macos")]
        if let Some(png) = macos_nspasteboard::read_image_png() {
            return Some(png);
        }
        if let Some(cb) = self.inner.as_mut() {
            if let Ok(img) = cb.get_image() {
                if validate_image_size(img.width, img.height).is_err() {
                    return None;
                }
                if let Ok(png) = rgba_to_png(img.width, img.height, &img.bytes) {
                    return Some(png);
                }
            }
        }
        None
    }

    fn image_clip_from_paths(&mut self, paths: Vec<PathBuf>, concealed: bool) -> Option<Clip> {
        let identity = file_identity(&paths)?;
        if self.file_identity.as_deref() == Some(&identity) {
            return self.file_clip.clone();
        }
        let png = png_from_single_image_file(&paths)?;
        let clip = Clip {
            mime: "image/png".into(),
            bytes: png,
            concealed,
        };
        self.file_identity = Some(identity);
        self.file_clip = Some(clip.clone());
        Some(clip)
    }

    fn read_local_file_paths(&mut self) -> Option<Vec<PathBuf>> {
        if let Some(paths) = wl_paste_file_paths().or_else(xclip_paste_file_paths) {
            if !paths.is_empty() {
                return Some(paths);
            }
        }
        #[cfg(target_os = "macos")]
        {
            let paths = macos_nspasteboard::read_file_paths();
            if !paths.is_empty() {
                return Some(paths);
            }
        }
        None
    }

    fn clear_file_cache(&mut self) {
        self.file_identity = None;
        self.file_clip = None;
    }

    fn set_text(&mut self, text: &str) -> Result<()> {
        let mut ok = false;
        if let Some(cb) = self.inner.as_mut() {
            if cb.set_text(text.to_string()).is_ok() {
                ok = true;
            }
        }
        if wl_copy_text(text).is_ok() {
            ok = true;
        }
        if xclip_copy_text(text).is_ok() {
            ok = true;
        }
        if pb_copy_text(text).is_ok() {
            ok = true;
        }
        if ok {
            Ok(())
        } else {
            anyhow::bail!("could not write clipboard text")
        }
    }

    fn set_image(&mut self, png: &[u8]) -> Result<()> {
        let (w, h, rgba) = png_to_rgba(png)?;
        let mut ok = false;
        if let Some(cb) = self.inner.as_mut() {
            let data = ImageData {
                width: w,
                height: h,
                bytes: Cow::Borrowed(&rgba),
            };
            if cb.set_image(data).is_ok() {
                ok = true;
            }
        }
        if wl_copy_image(png).is_ok() {
            ok = true;
        }
        if xclip_copy_image(png).is_ok() {
            ok = true;
        }
        #[cfg(target_os = "macos")]
        if macos_nspasteboard::write_image_png(png).is_ok() {
            ok = true;
        }
        if ok {
            Ok(())
        } else {
            anyhow::bail!("could not write clipboard image")
        }
    }

    fn clear(&mut self) -> Result<()> {
        let mut ok = false;
        if let Some(cb) = self.inner.as_mut() {
            if cb.clear().is_ok() {
                ok = true;
            }
        }
        if wl_clear().is_ok() {
            ok = true;
        }
        if xclip_clear().is_ok() {
            ok = true;
        }
        if pb_clear().is_ok() {
            ok = true;
        }
        if ok {
            Ok(())
        } else {
            anyhow::bail!("could not clear clipboard")
        }
    }
}

fn same_content(left: &Clip, right: &Clip) -> bool {
    left.mime == right.mime && left.bytes == right.bytes
}

fn normalize_clipboard_text(text: &str) -> String {
    text.trim_end_matches('\0').to_string()
}

fn decode_clipboard_bytes(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
        return None;
    }
    if bytes.starts_with(&[0xFF, 0xFE]) && bytes.len() >= 4 && bytes.len() % 2 == 0 {
        if let Ok(text) = decode_utf16_le(&bytes[2..]) {
            return Some(text);
        }
    }
    if looks_like_utf16_le(bytes) {
        if let Ok(text) = decode_utf16_le(bytes) {
            return Some(text);
        }
    }
    std::str::from_utf8(bytes).ok().map(ToString::to_string)
}

fn looks_like_utf16_le(bytes: &[u8]) -> bool {
    bytes.len() >= 4
        && bytes.len() % 2 == 0
        && bytes
            .chunks_exact(2)
            .take(8)
            .all(|chunk| chunk[1] == 0 && chunk[0] != 0)
}

fn decode_utf16_le(bytes: &[u8]) -> Result<String, std::string::FromUtf16Error> {
    let units = bytes
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    String::from_utf16(&units)
}

fn file_identity(paths: &[PathBuf]) -> Option<String> {
    if paths.len() != 1 {
        return None;
    }
    let path = &paths[0];
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    Some(format!("{}:{}:{modified}", path.display(), metadata.len()))
}

fn png_from_single_image_file(paths: &[PathBuf]) -> Option<Vec<u8>> {
    if paths.len() != 1 {
        return None;
    }
    let path = paths[0].canonicalize().unwrap_or_else(|_| paths[0].clone());
    if !is_image_path(&path) {
        return None;
    }
    let data = std::fs::read(&path).ok()?;
    let name = path.file_name()?.to_str()?;
    image_file_to_png(name, &data)
}

fn image_file_to_png(name: &str, data: &[u8]) -> Option<Vec<u8>> {
    if data.is_empty() || data.len() > MAX_PAYLOAD_BYTES {
        return None;
    }
    if let Some(png) = bytes_to_png(data) {
        return Some(png);
    }
    if is_png_name(name) && png_to_rgba(data).is_ok() {
        return Some(data.to_vec());
    }
    None
}

fn is_image_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(is_image_name)
}

fn is_image_name(name: &str) -> bool {
    matches!(
        Path::new(name)
            .extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff")
    )
}

fn is_png_name(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("png"))
}

fn parse_uri_list(raw: &str) -> Vec<PathBuf> {
    raw.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(file_uri_to_path)
        .collect()
}

fn parse_gnome_copied_files(raw: &str) -> Vec<PathBuf> {
    let mut lines = raw.lines().map(str::trim).filter(|line| !line.is_empty());
    let action = lines.next().unwrap_or("copy");
    if action != "copy" && action != "cut" {
        return Vec::new();
    }
    lines.filter_map(file_uri_to_path).collect()
}

fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let uri = uri.trim();
    let path = uri.strip_prefix("file://")?;
    let path = if cfg!(windows) {
        path.strip_prefix('/').unwrap_or(path)
    } else {
        path
    };
    let path = percent_decode(path)?;
    let path = PathBuf::from(path);
    if path.is_absolute() {
        Some(path)
    } else {
        None
    }
}

fn percent_decode(input: &str) -> Option<String> {
    let mut out = Vec::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(
                std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or(""),
                16,
            ) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(out).ok()
}

fn bytes_to_png(bytes: &[u8]) -> Option<Vec<u8>> {
    let img = image::load_from_memory(bytes).ok()?;
    let w = img.width() as usize;
    let h = img.height() as usize;
    validate_image_size(w, h).ok()?;
    rgba_to_png(w, h, &img.into_rgba8().into_raw()).ok()
}

fn detect_backend(arboard: bool) -> String {
    let mut parts = Vec::new();
    if arboard {
        parts.push("arboard");
    }
    if has_cmd("wl-copy") {
        parts.push("wl-clipboard");
    }
    if has_cmd("xclip") {
        parts.push("xclip");
    }
    if cfg!(target_os = "macos") {
        parts.push("nspasteboard");
    }
    if parts.is_empty() {
        "none".into()
    } else {
        parts.join("+")
    }
}

fn has_cmd(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|p| p.join(name).is_file()))
}

/// Wait until a Wayland compositor is reachable. systemd user services can
/// start at login before niri/GNOME exports `WAYLAND_DISPLAY`.
pub fn wait_for_display() {
    #[cfg(target_os = "linux")]
    {
        if apply_wayland_env() {
            return;
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            thread::sleep(Duration::from_millis(250));
            if apply_wayland_env() {
                return;
            }
        }
    }
}

fn apply_wayland_env() -> bool {
    if std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty()) {
        return true;
    }
    match discover_wayland_socket(None) {
        Some(display) => {
            std::env::set_var("WAYLAND_DISPLAY", &display);
            true
        }
        None => false,
    }
}

fn wayland_display() -> Option<String> {
    match std::env::var("WAYLAND_DISPLAY") {
        Ok(value) if !value.is_empty() => Some(value),
        _ => discover_wayland_socket(None),
    }
}

fn is_wayland() -> bool {
    wayland_display().is_some()
}

fn discover_wayland_socket(runtime_dir: Option<&Path>) -> Option<String> {
    let runtime = runtime_dir
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from))?;
    let mut found = Vec::new();
    for entry in fs::read_dir(&runtime).ok()?.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !is_wayland_socket_name(name) {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileTypeExt;
            if !file_type.is_socket() {
                continue;
            }
        }
        #[cfg(not(unix))]
        {
            let _ = file_type;
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(UNIX_EPOCH);
        found.push((modified, name.to_string()));
    }
    found.sort_by(|a, b| b.0.cmp(&a.0));
    found.into_iter().map(|(_, name)| name).next()
}

fn is_wayland_socket_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("wayland-") else {
        return false;
    };
    !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit())
}

fn wl_cmd(bin: &str) -> Option<Command> {
    if !is_wayland() || !has_cmd(bin) {
        return None;
    }
    let mut command = Command::new(bin);
    if let Some(display) = wayland_display() {
        command.env("WAYLAND_DISPLAY", display);
    }
    Some(command)
}

fn rgba_to_png(width: usize, height: usize, bytes: &[u8]) -> Result<Vec<u8>> {
    validate_image_size(width, height)?;
    let img: RgbaImage = ImageBuffer::from_raw(width as u32, height as u32, bytes.to_vec())
        .context("invalid rgba buffer")?;
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, ImageFormat::Png)?;
    Ok(out.into_inner())
}

fn png_to_rgba(png: &[u8]) -> Result<(usize, usize, Vec<u8>)> {
    let (width, height) =
        ImageReader::with_format(Cursor::new(png), ImageFormat::Png).into_dimensions()?;
    validate_image_size(width as usize, height as usize)?;
    let img = image::load_from_memory(png)?.into_rgba8();
    let w = img.width() as usize;
    let h = img.height() as usize;
    validate_image_size(w, h)?;
    Ok((w, h, img.into_raw()))
}

fn validate_image_size(width: usize, height: usize) -> Result<()> {
    let decoded_bytes = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .context("image dimensions overflow")?;
    if width > MAX_IMAGE_DIMENSION
        || height > MAX_IMAGE_DIMENSION
        || decoded_bytes > MAX_IMAGE_DECODED_BYTES
    {
        anyhow::bail!("image dimensions exceed the safe clipboard limit");
    }
    Ok(())
}

fn wl_paste_text() -> Option<String> {
    for mime in [
        "text/plain",
        "text/plain;charset=utf-8",
        "TEXT",
        "STRING",
        "UTF8_STRING",
    ] {
        let mut command = wl_cmd("wl-paste")?;
        command.args(["-t", mime, "--no-newline"]);
        if let Some(bytes) = read_command_output(command, MAX_PAYLOAD_BYTES) {
            if let Some(text) = decode_clipboard_bytes(&bytes) {
                let text = normalize_clipboard_text(&text);
                if !text.is_empty() {
                    return Some(text);
                }
            }
        }
    }
    None
}

fn wl_paste_image() -> Option<Vec<u8>> {
    for mime in IMAGE_PASTE_MIMES {
        let mut command = wl_cmd("wl-paste")?;
        command.args(["-t", mime]);
        let Some(output) = read_command_output(command, MAX_PAYLOAD_BYTES) else {
            continue;
        };
        if output.is_empty() {
            continue;
        }
        if *mime == "image/png" {
            if png_to_rgba(&output).is_ok() {
                return Some(output);
            }
        }
        if let Some(png) = bytes_to_png(&output) {
            return Some(png);
        }
    }
    None
}

fn wl_paste_file_paths() -> Option<Vec<PathBuf>> {
    let mut command = wl_cmd("wl-paste")?;
    command.args(["-t", "x-special/gnome-copied-files"]);
    if let Some(output) = read_command_output(command, 64 * 1024) {
        if let Some(text) = decode_clipboard_bytes(&output) {
            let paths = parse_gnome_copied_files(&text);
            if !paths.is_empty() {
                return Some(paths);
            }
        }
    }
    let mut command = wl_cmd("wl-paste")?;
    command.args(["-t", "text/uri-list"]);
    let output = read_command_output(command, 64 * 1024)?;
    let text = decode_clipboard_bytes(&output)?;
    let paths = parse_uri_list(&text);
    (!paths.is_empty()).then_some(paths)
}

fn wl_copy_text(text: &str) -> Result<()> {
    let mut child = wl_cmd("wl-copy")
        .ok_or_else(|| anyhow::anyhow!("no wl-copy"))?
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
    }
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("wl-copy failed")
    }
}

fn wl_copy_image(png: &[u8]) -> Result<()> {
    let mut child = wl_cmd("wl-copy")
        .ok_or_else(|| anyhow::anyhow!("no wl-copy"))?
        .args(["-t", "image/png"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(png)?;
    }
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("wl-copy image failed")
    }
}

fn wl_clear() -> Result<()> {
    let status = wl_cmd("wl-copy")
        .ok_or_else(|| anyhow::anyhow!("no wl-copy"))?
        .arg("--clear")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("wl-copy clear failed")
    }
}

fn xclip_paste_text() -> Option<String> {
    if is_wayland() || !has_cmd("xclip") {
        return None;
    }
    let mut command = Command::new("xclip");
    command.args(["-selection", "clipboard", "-o"]);
    let bytes = read_command_output(command, MAX_PAYLOAD_BYTES)?;
    decode_clipboard_bytes(&bytes).map(|text| normalize_clipboard_text(&text))
}

fn xclip_paste_image() -> Option<Vec<u8>> {
    if is_wayland() || !has_cmd("xclip") {
        return None;
    }
    for mime in IMAGE_PASTE_MIMES {
        let mut command = Command::new("xclip");
        command.args(["-selection", "clipboard", "-t", mime, "-o"]);
        let Some(output) = read_command_output(command, MAX_PAYLOAD_BYTES) else {
            continue;
        };
        if output.is_empty() {
            continue;
        }
        if *mime == "image/png" {
            if png_to_rgba(&output).is_ok() {
                return Some(output);
            }
        }
        if let Some(png) = bytes_to_png(&output) {
            return Some(png);
        }
    }
    None
}

fn xclip_paste_file_paths() -> Option<Vec<PathBuf>> {
    if is_wayland() || !has_cmd("xclip") {
        return None;
    }
    let mut command = Command::new("xclip");
    command.args(["-selection", "clipboard", "-t", "text/uri-list", "-o"]);
    let output = read_command_output(command, 64 * 1024)?;
    let text = decode_clipboard_bytes(&output)?;
    let paths = parse_uri_list(&text);
    (!paths.is_empty()).then_some(paths)
}

fn xclip_copy_text(text: &str) -> Result<()> {
    if is_wayland() || !has_cmd("xclip") {
        anyhow::bail!("no xclip");
    }
    let mut child = Command::new("xclip")
        .args(["-selection", "clipboard"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
    }
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("xclip failed")
    }
}

fn xclip_copy_image(png: &[u8]) -> Result<()> {
    if is_wayland() || !has_cmd("xclip") {
        anyhow::bail!("no xclip");
    }
    let mut child = Command::new("xclip")
        .args(["-selection", "clipboard", "-t", "image/png"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(png)?;
    }
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("xclip image failed")
    }
}

fn xclip_clear() -> Result<()> {
    if is_wayland() || !has_cmd("xclip") {
        anyhow::bail!("no xclip");
    }
    xclip_copy_text("")
}

fn pb_paste_text() -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let command = Command::new("pbpaste");
    String::from_utf8(read_command_output(command, MAX_PAYLOAD_BYTES)?).ok()
}

fn pb_copy_text(text: &str) -> Result<()> {
    if !cfg!(target_os = "macos") {
        anyhow::bail!("not macos");
    }
    let mut child = Command::new("pbcopy")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
    }
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("pbcopy failed")
    }
}

fn pb_clear() -> Result<()> {
    if !cfg!(target_os = "macos") {
        anyhow::bail!("not macos");
    }
    pb_copy_text("")
}

fn read_command_output(mut command: Command, max_bytes: usize) -> Option<Vec<u8>> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = thread::spawn(move || {
        let mut output = Vec::new();
        let result = stdout
            .by_ref()
            .take(max_bytes.saturating_add(1) as u64)
            .read_to_end(&mut output);
        (result, output)
    });
    let deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        if let Some(status) = child.try_wait().ok()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return None;
        }
        thread::sleep(Duration::from_millis(20));
    };
    let (read_result, output) = reader.join().ok()?;
    read_result.ok()?;
    if output.len() > max_bytes {
        return None;
    }
    status.success().then_some(output)
}

fn is_concealed() -> bool {
    #[cfg(target_os = "macos")]
    {
        macos_concealed()
    }
    #[cfg(target_os = "linux")]
    {
        linux_concealed()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        false
    }
}

#[cfg(target_os = "linux")]
fn linux_concealed() -> bool {
    let command = if let Some(mut command) = wl_cmd("wl-paste") {
        command.arg("--list-types");
        command
    } else if !is_wayland() && has_cmd("xclip") {
        let mut command = Command::new("xclip");
        command.args(["-selection", "clipboard", "-t", "TARGETS", "-o"]);
        command
    } else {
        // If the active Linux backend cannot expose clipboard metadata, do not
        // risk forwarding password-manager contents.
        return true;
    };
    let Some(output) = read_command_output(command, 64 * 1024) else {
        return true;
    };
    let types = String::from_utf8_lossy(&output);
    types.lines().any(|t| {
        let t = t.to_ascii_lowercase();
        t.contains("passwordmanagerhint")
            || t.contains("x-kde-passwordmanagerhint")
            || t.contains("concealed")
    })
}

#[cfg(target_os = "macos")]
fn macos_concealed() -> bool {
    macos_nspasteboard::is_concealed()
}

#[cfg(target_os = "macos")]
mod macos_nspasteboard {
    use anyhow::Result;
    use objc2_app_kit::NSPasteboard;
    use objc2_foundation::{ns_string, NSArray, NSData, NSString};

    pub fn is_concealed() -> bool {
        let marker = ns_string!("org.nspasteboard.ConcealedType");
        let pb = NSPasteboard::generalPasteboard();
        match pb.types() {
            Some(types) => types.containsObject(marker),
            None => false,
        }
    }

    pub fn read_image_png() -> Option<Vec<u8>> {
        let pb = NSPasteboard::generalPasteboard();
        for mime in [
            "public.png",
            "Apple PNG pasteboard type",
            "public.tiff",
            "public.jpeg",
        ] {
            let ty = NSString::from_str(mime);
            let Some(data) = pb.dataForType(&ty) else {
                continue;
            };
            let bytes = data.to_vec();
            if bytes.is_empty() {
                continue;
            }
            if mime.contains("png") {
                return Some(bytes);
            }
            if let Some(png) = super::bytes_to_png(&bytes) {
                return Some(png);
            }
        }
        None
    }

    pub fn write_image_png(png: &[u8]) -> Result<()> {
        let pb = NSPasteboard::generalPasteboard();
        pb.clearContents();
        let data = NSData::with_bytes(png);
        if pb.setData_forType(Some(&data), ns_string!("public.png")) {
            Ok(())
        } else {
            anyhow::bail!("could not write image to NSPasteboard")
        }
    }

    pub fn read_file_paths() -> Vec<std::path::PathBuf> {
        let pb = NSPasteboard::generalPasteboard();
        if let Some(list) = pb.propertyListForType(ns_string!("NSFilenamesPboardType")) {
            let mut paths = Vec::new();
            if let Some(array) = list.downcast_ref::<NSArray>() {
                for elem in array {
                    if let Some(item) = elem.downcast_ref::<NSString>() {
                        paths.push(std::path::PathBuf::from(item.to_string()));
                    }
                }
            }
            if !paths.is_empty() {
                return paths;
            }
        }
        if let Some(url) = pb.stringForType(ns_string!("public.file-url")) {
            if let Some(path) = super::file_uri_to_path(&url.to_string()) {
                return vec![path];
            }
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        decode_clipboard_bytes, file_uri_to_path, is_image_name, parse_gnome_copied_files,
        parse_uri_list, png_from_single_image_file, same_content, Clip,
    };
    use image::{ImageBuffer, ImageFormat, RgbaImage};
    use std::io::Cursor;

    fn clip(mime: &str, bytes: &[u8]) -> Clip {
        Clip {
            mime: mime.into(),
            bytes: bytes.to_vec(),
            concealed: false,
        }
    }

    #[test]
    fn expiry_match_requires_exact_text_content() {
        let expected = clip("text/plain", b"remote text");
        assert!(same_content(&expected, &clip("text/plain", b"remote text")));
        assert!(!same_content(&expected, &clip("text/plain", b"user edit")));
        assert!(!same_content(&expected, &clip("image/png", b"remote text")));
    }

    #[test]
    fn expiry_match_requires_exact_image_bytes() {
        let expected = clip("image/png", &[1, 2, 3, 4]);
        assert!(same_content(&expected, &clip("image/png", &[1, 2, 3, 4])));
        assert!(!same_content(&expected, &clip("image/png", &[1, 2, 3, 5])));
    }

    #[test]
    fn parses_uri_lists_and_gnome_file_payloads() {
        let paths = parse_uri_list("file:///tmp/a.png\n#comment\n");
        assert_eq!(paths, vec![std::path::PathBuf::from("/tmp/a.png")]);
        let paths = parse_gnome_copied_files("copy\nfile:///tmp/a.png\nfile:///tmp/b.txt\n");
        assert_eq!(
            paths,
            vec![
                std::path::PathBuf::from("/tmp/a.png"),
                std::path::PathBuf::from("/tmp/b.txt"),
            ]
        );
        assert_eq!(
            file_uri_to_path("file:///home/user/My%20Photo.png"),
            Some(std::path::PathBuf::from("/home/user/My Photo.png"))
        );
        assert_eq!(
            file_uri_to_path("file:///home/user/Caf%C3%A9.png"),
            Some(std::path::PathBuf::from("/home/user/Café.png"))
        );
    }

    #[test]
    fn decodes_utf16_clipboard_text() {
        let utf16 = "hi"
            .encode_utf16()
            .flat_map(|unit| unit.to_le_bytes())
            .collect::<Vec<_>>();
        assert_eq!(decode_clipboard_bytes(&utf16), Some("hi".into()));
    }

    #[test]
    fn single_image_file_becomes_png_pixels() {
        assert!(is_image_name("vacation.jpg"));
        assert!(is_image_name("shot.PNG"));
        assert!(!is_image_name("notes.pdf"));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photo.png");
        let img: RgbaImage = ImageBuffer::from_pixel(1, 1, image::Rgba([1, 2, 3, 255]));
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, ImageFormat::Png).unwrap();
        std::fs::write(&path, out.into_inner()).unwrap();

        let decoded = png_from_single_image_file(&[path.clone()]).unwrap();
        assert!(!decoded.is_empty());
        assert!(png_from_single_image_file(&[path.clone(), path]).is_none());

        let pdf = dir.path().join("notes.pdf");
        std::fs::write(&pdf, b"%PDF").unwrap();
        assert!(png_from_single_image_file(&[pdf]).is_none());
    }

    #[test]
    fn wayland_socket_names_are_numeric() {
        assert!(super::is_wayland_socket_name("wayland-0"));
        assert!(super::is_wayland_socket_name("wayland-1"));
        assert!(!super::is_wayland_socket_name("wayland-1.lock"));
        assert!(!super::is_wayland_socket_name("pulse"));
    }

    #[cfg(unix)]
    #[test]
    fn discovers_wayland_socket_in_runtime_dir() {
        use std::os::unix::net::UnixListener;
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("wayland-1");
        let _listener = UnixListener::bind(&sock).unwrap();
        std::fs::write(dir.path().join("wayland-1.lock"), b"").unwrap();
        assert_eq!(
            super::discover_wayland_socket(Some(dir.path())).as_deref(),
            Some("wayland-1")
        );
    }
}
