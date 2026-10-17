#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn cli_reports_tailscale_and_service_failures_with_nonzero_exit_codes() {
    let temp = tempfile::tempdir().unwrap();
    let bin_dir = temp.path().join("bin");
    fs::create_dir(&bin_dir).unwrap();
    for (name, script) in [
        (
            "tailscale",
            "#!/bin/sh\necho 'permission denied tskey-private' >&2\nexit 1\n",
        ),
        ("systemctl", "#!/bin/sh\nexit 1\n"),
        ("xclip", "#!/bin/sh\nexit 0\n"),
    ] {
        let path = bin_dir.join(name);
        fs::write(&path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let run = |arg| {
        Command::new(env!("CARGO_BIN_EXE_pastebridge"))
            .arg(arg)
            .env("HOME", temp.path())
            .env("PASTEBRIDGE_HOME", temp.path().join("config"))
            .env("XDG_CONFIG_HOME", temp.path().join("config"))
            .env("XDG_RUNTIME_DIR", temp.path())
            .env("PATH", format!("{}:/usr/bin:/bin", bin_dir.display()))
            .env("DISPLAY", ":99999")
            .env_remove("WAYLAND_DISPLAY")
            .output()
            .unwrap()
    };
    let doctor = run("doctor");
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&doctor.stdout),
        String::from_utf8_lossy(&doctor.stderr)
    );
    assert!(!doctor.status.success());
    assert!(
        output.contains("permission denied by Tailscale"),
        "{output}"
    );
    assert!(
        output.contains("stopped; run pastebridge install-service"),
        "{output}"
    );
    assert!(!output.contains("tskey-private"));
    let install = run("install-service");
    assert!(!install.status.success());
    assert!(String::from_utf8_lossy(&install.stderr).contains("daemon-reload failed"));
    assert!(!String::from_utf8_lossy(&install.stdout).contains("enabled and ready"));
}
