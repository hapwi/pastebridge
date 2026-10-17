#!/usr/bin/env python3
"""Exercise the piped installer against local releases in disposable homes."""

import hashlib
import http.server
import io
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import threading
import unittest


INSTALLER = (Path(__file__).resolve().parents[1] / "install.sh").read_text()


def binary(version="1.2.3", fail_service=False):
    return f"""#!/bin/sh
case "$1" in
  --version) printf 'pastebridge {version}\\n' ;;
  install-service) printf 'service {version}\\n' >> "$FIXTURE_LOG"; exit {int(fail_service)} ;;
  uninstall-service) printf 'uninstall\\n' >> "$FIXTURE_LOG" ;;
  list) printf 'no paired devices\\n' ;;
  pair) printf 'pair\\n' >> "$FIXTURE_LOG" ;;
  *) exit 2 ;;
esac
""".encode()


class InstallerTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="pastebridge-installer-")
        self.root = Path(self.temp.name)
        self.home = self.root / "home"
        self.home.mkdir()
        self.tools = self.root / "tools"
        self.tools.mkdir()
        self.install_dir = self.home / "my tools' bin"
        self.log = self.root / "calls"
        self.responses = {}
        test = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                status, body = test.responses.get(self.path, (404, b"not found"))
                self.send_response(status)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *_):
                pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.env = {
            **os.environ,
            "HOME": str(self.home),
            "XDG_CONFIG_HOME": str(self.home / ".config"),
            "CARGO_HOME": str(self.home / ".cargo"),
            "PATH": f"{self.tools}{os.pathsep}{os.environ['PATH']}",
            "SHELL": "/bin/zsh",
            "BASH_COMPAT": "3.2",
            "TMPDIR": str(self.root),
            "PASTEBRIDGE_INSTALL_DIR": str(self.install_dir),
            "PASTEBRIDGE_RELEASE": "v1.2.3",
            "PASTEBRIDGE_RELEASE_BASE_URL": f"http://127.0.0.1:{self.server.server_port}",
            "PASTEBRIDGE_NO_PAIR": "1",
            "FIXTURE_LOG": str(self.log),
        }
        for key in ["WAYLAND_DISPLAY", "DISPLAY", "ZDOTDIR", "PASTEBRIDGE_FROM_SOURCE"]:
            self.env.pop(key, None)

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()
        self.temp.cleanup()

    def release(self, os_name="Linux", arch="x86_64", payload=None):
        uname = self.tools / "uname"
        uname.write_text(f'#!/bin/sh\ncase "$1" in -s) echo {os_name};; -m) echo {arch};; esac\n')
        uname.chmod(0o755)
        target = {
            ("Linux", "x86_64"): "x86_64-unknown-linux-gnu",
            ("Linux", "aarch64"): "aarch64-unknown-linux-gnu",
            ("Darwin", "arm64"): "aarch64-apple-darwin",
        }[(os_name, arch)]
        payload = binary() if payload is None else payload
        output = io.BytesIO()
        with tarfile.open(fileobj=output, mode="w:gz") as archive:
            member = tarfile.TarInfo("pastebridge")
            member.size = len(payload)
            member.mode = 0o755
            archive.addfile(member, io.BytesIO(payload))
        archive_name = f"pastebridge-{target}.tar.gz"
        content = output.getvalue()
        self.responses[f"/v1.2.3/{archive_name}"] = (200, content)
        checksum = hashlib.sha256(content).hexdigest()
        self.responses["/v1.2.3/SHA256SUMS"] = (200, f"{checksum}  {archive_name}\n".encode())

    def run_installer(self):
        return subprocess.run(
            ["bash"], input=INSTALLER, text=True, env=self.env,
            cwd=self.root, capture_output=True, timeout=30,
        )

    def previous_install(self):
        self.install_dir.mkdir(parents=True)
        executable = self.install_dir / "pastebridge"
        executable.write_bytes(binary("1.0.0"))
        executable.chmod(0o755)
        return executable

    def test_install_and_rerun_preserve_fresh_shell_path(self):
        self.release()
        for _ in range(2):
            result = self.run_installer()
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual((self.install_dir / "pastebridge").read_bytes(), binary())
        self.assertEqual(self.log.read_text(), "service 1.2.3\nservice 1.2.3\n")
        for name in [".zshrc", ".zprofile"]:
            profile = self.home / name
            self.assertEqual(profile.read_text().count("export PATH="), 1)
            result = subprocess.run(
                ["bash", "-c", '. "$1"; command -v pastebridge', "bash", str(profile)],
                env=self.env, capture_output=True, text=True,
            )
            self.assertEqual(result.returncode, 0, profile.read_text() + result.stdout + result.stderr)
            self.assertEqual(result.stdout.strip(), str(self.install_dir / "pastebridge"))

    def test_selects_each_supported_platform(self):
        for os_name, arch in [("Linux", "x86_64"), ("Linux", "aarch64"), ("Darwin", "arm64")]:
            with self.subTest(os_name=os_name, arch=arch):
                self.release(os_name, arch)
                result = self.run_installer()
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_rejects_intel_mac_before_changing_previous_install(self):
        executable = self.previous_install()
        uname = self.tools / "uname"
        uname.write_text('#!/bin/sh\ncase "$1" in -s) echo Darwin;; -m) echo x86_64;; esac\n')
        uname.chmod(0o755)
        for from_source in ["0", "1"]:
            with self.subTest(from_source=from_source):
                self.env["PASTEBRIDGE_FROM_SOURCE"] = from_source
                result = self.run_installer()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("requires Apple Silicon", result.stderr)
                self.assertEqual(executable.read_bytes(), binary("1.0.0"))
                self.assertFalse(self.log.exists())

    def test_cargo_bin_can_also_be_the_install_directory(self):
        self.install_dir = self.home / ".cargo" / "bin"
        self.env["PASTEBRIDGE_INSTALL_DIR"] = str(self.install_dir)
        self.release()
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        executable = self.install_dir / "pastebridge"
        self.assertFalse(executable.is_symlink())
        self.assertEqual(executable.read_bytes(), binary())

    def test_missing_or_invalid_checksums_leave_previous_install(self):
        executable = self.previous_install()
        for manifest in [(404, b"missing"), (200, b"invalid manifest"), (200, b"0" * 64 + b"  pastebridge-x86_64-unknown-linux-gnu.tar.gz\n")]:
            self.release()
            self.responses["/v1.2.3/SHA256SUMS"] = manifest
            result = self.run_installer()
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(executable.read_bytes(), binary("1.0.0"))
            self.assertFalse(self.log.exists())

    def test_failed_download_does_not_fall_back_to_source(self):
        executable = self.previous_install()
        self.release()
        self.responses["/v1.2.3/pastebridge-x86_64-unknown-linux-gnu.tar.gz"] = (404, b"missing")
        result = self.run_installer()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(executable.read_bytes(), binary("1.0.0"))
        self.assertNotIn("source build", result.stdout)

    def test_wrong_version_is_rejected_before_activation(self):
        executable = self.previous_install()
        self.release(payload=binary("9.9.9"))
        result = self.run_installer()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(executable.read_bytes(), binary("1.0.0"))

    def test_service_failure_restores_previous_executable(self):
        executable = self.previous_install()
        self.release(payload=binary(fail_service=True))
        result = self.run_installer()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(executable.read_bytes(), binary("1.0.0"))
        self.assertEqual(self.log.read_text(), "service 1.2.3\nservice 1.0.0\n")
        self.assertEqual(list(self.install_dir.glob(".pastebridge-install.*")), [])

    def test_failed_first_install_removes_its_service_and_executable(self):
        self.release(payload=binary(fail_service=True))
        result = self.run_installer()
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.install_dir / "pastebridge").exists())
        self.assertEqual(self.log.read_text(), "service 1.2.3\nuninstall\n")


if __name__ == "__main__":
    unittest.main()
