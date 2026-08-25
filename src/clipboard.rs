use anyhow::{Context, Result};
use arboard::{Clipboard as Arboard, ImageData};
use image::{ImageBuffer, ImageFormat, ImageReader, RgbaImage};
use std::borrow::Cow;
use std::io::Cursor;
use std::io::Read;
use std::io::Write;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::MAX_PAYLOAD_BYTES;
use crate::crypto::clip_hash;

const MAX_IMAGE_DIMENSION: usize = 8192;
const MAX_IMAGE_DECODED_BYTES: usize = 64 * 1024 * 1024;

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
    pub backend: String,
}

impl Clipboard {
    pub fn open() -> Result<Self> {
        let inner = Arboard::new().ok();
        let backend = detect_backend(inner.is_some());
        Ok(Self {
            inner,
            last_hash: None,
            ignore_hash: None,
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
        if let Some(text) = self.read_text() {
            let text = text.trim_end_matches('\0').to_string();
            if !text.is_empty() {
                return Some(Clip {
                    mime: "text/plain".into(),
                    bytes: text.into_bytes(),
                    concealed,
                });
            }
        }
        if let Some(png) = self.read_image_png() {
            if !png.is_empty() {
                return Some(Clip {
                    mime: "image/png".into(),
                    bytes: png,
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

fn is_wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
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
    if !is_wayland() || !has_cmd("wl-paste") {
        return None;
    }
    let mut command = Command::new("wl-paste");
    command.args(["-t", "text", "--no-newline"]);
    String::from_utf8(read_command_output(command, MAX_PAYLOAD_BYTES)?).ok()
}

fn wl_paste_image() -> Option<Vec<u8>> {
    if !is_wayland() || !has_cmd("wl-paste") {
        return None;
    }
    let mut command = Command::new("wl-paste");
    command.args(["-t", "image/png"]);
    let output = read_command_output(command, MAX_PAYLOAD_BYTES)?;
    (!output.is_empty()).then_some(output)
}

fn wl_copy_text(text: &str) -> Result<()> {
    if !is_wayland() || !has_cmd("wl-copy") {
        anyhow::bail!("no wl-copy");
    }
    let mut child = Command::new("wl-copy")
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
    if !is_wayland() || !has_cmd("wl-copy") {
        anyhow::bail!("no wl-copy");
    }
    let mut child = Command::new("wl-copy")
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
    if !is_wayland() || !has_cmd("wl-copy") {
        anyhow::bail!("no wl-copy");
    }
    let status = Command::new("wl-copy")
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
    String::from_utf8(read_command_output(command, MAX_PAYLOAD_BYTES)?).ok()
}

fn xclip_paste_image() -> Option<Vec<u8>> {
    if is_wayland() || !has_cmd("xclip") {
        return None;
    }
    let mut command = Command::new("xclip");
    command.args(["-selection", "clipboard", "-t", "image/png", "-o"]);
    let output = read_command_output(command, MAX_PAYLOAD_BYTES)?;
    (!output.is_empty()).then_some(output)
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
    #[cfg(not(target_os = "macos"))]
    {
        linux_concealed()
    }
}

fn linux_concealed() -> bool {
    let command = if is_wayland() && has_cmd("wl-paste") {
        let mut command = Command::new("wl-paste");
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
    use objc2_app_kit::NSPasteboard;
    use objc2_foundation::ns_string;

    pub fn is_concealed() -> bool {
        let marker = ns_string!("org.nspasteboard.ConcealedType");
        unsafe {
            let pb = NSPasteboard::generalPasteboard();
            match pb.types() {
                Some(types) => types.containsObject(marker),
                None => false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{same_content, Clip};

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
}
