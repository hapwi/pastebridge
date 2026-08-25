use anyhow::Result;
use std::path::Path;

#[cfg(target_os = "macos")]
use anyhow::{bail, Context};
#[cfg(target_os = "macos")]
use std::process::Command;

#[cfg(target_os = "macos")]
const SIGN_IDENTITY: &str = "Pastebridge";
#[cfg(target_os = "macos")]
const BUNDLE_ID: &str = "dev.pastebridge.pastebridge";

#[cfg(target_os = "macos")]
#[used]
#[allow(dead_code)]
#[link_section = "__TEXT,__info_plist"]
static INFO_PLIST: [u8; include_bytes!("../macos/Info.plist").len()] =
    *include_bytes!("../macos/Info.plist");

/// Keep macOS Local Network permission attached to this install.
///
/// Each unsigned download has a new code-signing hash, so Sequoia treats it as
/// a different app and asks again. Re-sign with a per-machine certificate so
/// the designated requirement stays the same across updates.
pub fn prepare_executable(binary: &Path, config_dir: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        prepare_macos(binary, config_dir)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (binary, config_dir);
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn prepare_macos(binary: &Path, config_dir: &Path) -> Result<()> {
    let _ = &INFO_PLIST;
    strip_quarantine(binary);
    let keychain = config_dir.join("codesign.keychain-db");
    let password_file = config_dir.join("codesign.pass");
    let password = ensure_identity(config_dir, &keychain, &password_file)?;
    unlock_keychain(&keychain, &password)?;
    ensure_keychain_on_search_list(&keychain)?;
    codesign(binary, &keychain)?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn strip_quarantine(binary: &Path) {
    let _ = Command::new("xattr")
        .args(["-d", "com.apple.quarantine"])
        .arg(binary)
        .status();
}

#[cfg(target_os = "macos")]
fn ensure_identity(config_dir: &Path, keychain: &Path, password_file: &Path) -> Result<String> {
    std::fs::create_dir_all(config_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(config_dir, std::fs::Permissions::from_mode(0o700));
    }
    let password = if password_file.exists() {
        crate::config::read_private_file(password_file)?
            .trim()
            .to_string()
    } else {
        let password = random_password();
        crate::config::write_secret_file(password_file, &format!("{password}\n"))?;
        password
    };
    if !keychain.exists() || !identity_present(keychain, &password) {
        create_identity(config_dir, keychain, &password)?;
    }
    Ok(password)
}

#[cfg(target_os = "macos")]
fn identity_present(keychain: &Path, password: &str) -> bool {
    if unlock_keychain(keychain, password).is_err() {
        return false;
    }
    let output = Command::new("security")
        .args(["find-identity", "-v", "-p", "codesigning"])
        .arg(keychain)
        .output();
    output.is_ok_and(|out| {
        out.status.success() && String::from_utf8_lossy(&out.stdout).contains(SIGN_IDENTITY)
    })
}

#[cfg(target_os = "macos")]
fn create_identity(config_dir: &Path, keychain: &Path, password: &str) -> Result<()> {
    let _ = std::fs::remove_file(keychain);
    run(
        Command::new("security")
            .args(["create-keychain", "-p", password])
            .arg(keychain),
        "creating the Pastebridge signing keychain",
    )?;
    let _ = Command::new("security")
        .arg("set-keychain-settings")
        .arg(keychain)
        .status();

    let tmp = config_dir.join(".codesign-tmp");
    std::fs::create_dir_all(&tmp)?;
    let cnf = tmp.join("codesign.cnf");
    let key = tmp.join("key.pem");
    let cert = tmp.join("cert.pem");
    let p12 = tmp.join("identity.p12");
    std::fs::write(
        &cnf,
        "[req]\n\
         distinguished_name = req_distinguished_name\n\
         x509_extensions = v3_ext\n\
         prompt = no\n\
         \n\
         [req_distinguished_name]\n\
         CN = Pastebridge\n\
         \n\
         [v3_ext]\n\
         basicConstraints = CA:FALSE\n\
         keyUsage = critical, digitalSignature\n\
         extendedKeyUsage = critical, codeSigning\n",
    )?;
    let created = (|| {
        run(
            Command::new("/usr/bin/openssl")
                .args([
                    "req", "-new", "-x509", "-days", "3650", "-nodes", "-newkey", "rsa:2048",
                    "-keyout",
                ])
                .arg(&key)
                .arg("-out")
                .arg(&cert)
                .arg("-config")
                .arg(&cnf),
            "creating a local code-signing certificate",
        )?;
        run(
            Command::new("/usr/bin/openssl")
                .args(["pkcs12", "-export", "-inkey"])
                .arg(&key)
                .arg("-in")
                .arg(&cert)
                .arg("-out")
                .arg(&p12)
                .args([
                    "-passout",
                    &format!("pass:{password}"),
                    "-name",
                    SIGN_IDENTITY,
                ]),
            "exporting the local code-signing certificate",
        )?;
        run(
            Command::new("security")
                .args(["import"])
                .arg(&p12)
                .args(["-k"])
                .arg(keychain)
                .args([
                    "-P",
                    password,
                    "-T",
                    "/usr/bin/codesign",
                    "-T",
                    "/usr/bin/security",
                ]),
            "importing the local code-signing certificate",
        )?;
        let _ = Command::new("security")
            .args([
                "set-key-partition-list",
                "-S",
                "apple-tool:,apple:,codesign:",
                "-s",
                "-k",
                password,
            ])
            .arg(keychain)
            .status();
        ensure_keychain_on_search_list(keychain)?;
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    created
}

#[cfg(target_os = "macos")]
fn unlock_keychain(keychain: &Path, password: &str) -> Result<()> {
    run(
        Command::new("security")
            .args(["unlock-keychain", "-p", password])
            .arg(keychain),
        "unlocking the Pastebridge signing keychain",
    )
}

#[cfg(target_os = "macos")]
fn codesign(binary: &Path, keychain: &Path) -> Result<()> {
    run(
        Command::new("codesign")
            .args([
                "--force",
                "--sign",
                SIGN_IDENTITY,
                "--identifier",
                BUNDLE_ID,
            ])
            .arg("--keychain")
            .arg(keychain)
            .arg(binary),
        "signing Pastebridge so macOS keeps Local Network permission",
    )
}

#[cfg(target_os = "macos")]
fn ensure_keychain_on_search_list(keychain: &Path) -> Result<()> {
    let output = Command::new("security")
        .args(["list-keychains", "-d", "user"])
        .output()
        .context("listing keychains")?;
    let mut existing: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let line = line.trim().trim_matches('"');
            if line.is_empty() {
                None
            } else {
                Some(line.to_string())
            }
        })
        .collect();
    let path = keychain.display().to_string();
    if existing.iter().any(|item| item == &path) {
        return Ok(());
    }
    existing.insert(0, path);
    let mut cmd = Command::new("security");
    cmd.args(["list-keychains", "-d", "user", "-s"]);
    for item in &existing {
        cmd.arg(item);
    }
    run(
        &mut cmd,
        "adding the Pastebridge keychain to the search list",
    )
}

#[cfg(target_os = "macos")]
fn random_password() -> String {
    let mut bytes = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut bytes);
    hex::encode(bytes)
}

#[cfg(target_os = "macos")]
fn run(cmd: &mut Command, what: &str) -> Result<()> {
    let output = cmd.output().with_context(|| what.to_string())?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        bail!("{what} failed");
    }
    bail!("{what} failed: {stderr}")
}
