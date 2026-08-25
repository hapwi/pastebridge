use anyhow::{Context, Result};
use arboard::{Clipboard as Arboard, ImageData};
use image::{ImageBuffer, ImageFormat, RgbaImage};
use std::borrow::Cow;
use std::io::Cursor;
use std::io::Write;
use std::process::{Command, Stdio};

use crate::crypto::clip_hash;

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
        if let Some(cb) = self.inner.as_mut() {
            if let Ok(text) = cb.get_text() {
                if !text.is_empty() {
                    return Some(text);
                }
            }
        }
        wl_paste_text()
            .or_else(xclip_paste_text)
            .or_else(pb_paste_text)
    }

    fn read_image_png(&mut self) -> Option<Vec<u8>> {
        if let Some(cb) = self.inner.as_mut() {
            if let Ok(img) = cb.get_image() {
                if let Ok(png) = rgba_to_png(img.width, img.height, &img.bytes) {
                    return Some(png);
                }
            }
        }
        wl_paste_image().or_else(xclip_paste_image)
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
    let img: RgbaImage = ImageBuffer::from_raw(width as u32, height as u32, bytes.to_vec())
        .context("invalid rgba buffer")?;
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, ImageFormat::Png)?;
    Ok(out.into_inner())
}

fn png_to_rgba(png: &[u8]) -> Result<(usize, usize, Vec<u8>)> {
    let img = image::load_from_memory(png)?.into_rgba8();
    let w = img.width() as usize;
    let h = img.height() as usize;
    Ok((w, h, img.into_raw()))
}

fn wl_paste_text() -> Option<String> {
    if !is_wayland() || !has_cmd("wl-paste") {
        return None;
    }
    let out = Command::new("wl-paste")
        .args(["-t", "text", "--no-newline"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

fn wl_paste_image() -> Option<Vec<u8>> {
    if !is_wayland() || !has_cmd("wl-paste") {
        return None;
    }
    let out = Command::new("wl-paste")
        .args(["-t", "image/png"])
        .output()
        .ok()?;
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    Some(out.stdout)
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

fn xclip_paste_text() -> Option<String> {
    if is_wayland() || !has_cmd("xclip") {
        return None;
    }
    let out = Command::new("xclip")
        .args(["-selection", "clipboard", "-o"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

fn xclip_paste_image() -> Option<Vec<u8>> {
    if is_wayland() || !has_cmd("xclip") {
        return None;
    }
    let out = Command::new("xclip")
        .args(["-selection", "clipboard", "-t", "image/png", "-o"])
        .output()
        .ok()?;
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    Some(out.stdout)
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

fn pb_paste_text() -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let out = Command::new("pbpaste").output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
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
    if !has_cmd("wl-paste") {
        return false;
    }
    let Ok(out) = Command::new("wl-paste").arg("--list-types").output() else {
        return false;
    };
    let types = String::from_utf8_lossy(&out.stdout);
    types.lines().any(|t| {
        let t = t.to_ascii_lowercase();
        t.contains("passwordmanagerhint") || t.contains("concealed")
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
