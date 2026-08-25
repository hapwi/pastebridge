use anyhow::Result;
use std::path::Path;

#[cfg(target_os = "macos")]
use std::process::{Command, Stdio};

#[cfg(target_os = "macos")]
#[used]
#[allow(dead_code)]
#[link_section = "__TEXT,__info_plist"]
static INFO_PLIST: [u8; include_bytes!("../macos/Info.plist").len()] =
    *include_bytes!("../macos/Info.plist");

/// Clear quarantine so a GitHub download is not treated as a new Gatekeeper item.
///
/// Release binaries are already signed with the stable `hapwi Pastebridge`
/// identity in CI. Do not re-sign here — that would replace hapwi's cert with a
/// different key and reset Local Network permission.
pub fn prepare_executable(binary: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let _ = &INFO_PLIST;
        // Missing quarantine is normal for a previously-installed binary.
        // xattr prints to stderr in that case; swallow it.
        let _ = Command::new("xattr")
            .args(["-d", "com.apple.quarantine"])
            .arg(binary)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = binary;
        Ok(())
    }
}
