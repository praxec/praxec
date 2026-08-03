//! The [`InstallerIo`] seam — every host-touching operation the release
//! provider needs, injectable so the resolve → verify → place decision logic in
//! [`super::install_release`] is unit-tested with a fake, never a real network
//! or filesystem. Mirrors the [`crate::currency::CurrencyIo`] idiom (a trait +
//! a `Real*` production impl).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use super::InstallError;

/// Upper bound on the `<bin> --version` probe [`RealInstallerIo::installed_version`]
/// runs. `doctor` currency (managed-bin-dir awareness) now shells out to this
/// during a health check, so a binary that HANGS on `--version` must not hang
/// doctor. 5s covers a slow-but-live tool; a genuinely stuck probe is killed and
/// reads as `None` (fail-safe: unknown → reinstall / `CURRENCY_UNKNOWN`, never a
/// wrong "current").
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Run `cmd` to completion but abandon it after `timeout`: spawn (piped output),
/// poll [`std::process::Child::try_wait`], and `kill` the child on expiry —
/// returning `None` so a hung probe degrades to "unknown", never a wrong verdict.
/// `--version` output is tiny (well under a pipe buffer), so the poll never
/// deadlocks on a full pipe. No new dependency — plain std threads-of-execution
/// via a sleep-poll loop.
fn output_bounded(
    mut cmd: std::process::Command,
    timeout: Duration,
) -> Option<std::process::Output> {
    use std::process::Stdio;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            // Exited on its own — collect the (already-buffered) output.
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Total-request and connect bounds for release downloads. `reqwest`'s default
/// client has NO timeout, so a stalled release host would hang `praxec tools
/// install` / `doctor --fix` indefinitely; 60s covers a slow-but-live CDN asset
/// while the 10s connect bound fails fast on a dead host.
const HTTP_TIMEOUT: Duration = Duration::from_secs(60);
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The process-wide blocking HTTP client, built ONCE with an explicit timeout
/// (see [`HTTP_TIMEOUT`]). Reused across every `http_get` so no download can
/// hang unbounded. A builder failure surfaces as an [`InstallError::Io`].
fn http_client() -> Result<&'static reqwest::blocking::Client, InstallError> {
    static CLIENT: OnceLock<Result<reqwest::blocking::Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::blocking::Client::builder()
                .timeout(HTTP_TIMEOUT)
                .connect_timeout(HTTP_CONNECT_TIMEOUT)
                .build()
                .map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(|e| InstallError::Io(e.clone()))
}

/// The host-touching operations the provider chain needs. Kept minimal: an
/// HTTP GET (for the asset + its `checksums.sha256`), an executable placement,
/// an installed-version probe (idempotency), the praxec-managed bin dir, a
/// command-on-PATH probe (docker-provider availability), and a `docker pull`.
///
/// Unlike [`crate::currency::CurrencyIo`] — whose probes degrade to `None` — a
/// download/placement failure is *fatal* to an install, so the fallible methods
/// return a typed [`InstallError`] rather than an `Option`.
pub trait InstallerIo {
    /// Fetch the bytes at `url`. A non-2xx / unreachable / transport failure is
    /// an `Err`; [`super::install_release`] re-wraps it with the resolved URL +
    /// host triple so the message is self-describing regardless of the impl.
    fn http_get(&self, url: &str) -> Result<Vec<u8>, InstallError>;

    /// Write `bytes` as an executable named `name` into `dir`, returning the
    /// placed path. Creates `dir` if absent; sets the executable bit on unix.
    fn place_executable(
        &self,
        dir: &Path,
        name: &str,
        bytes: &[u8],
    ) -> Result<PathBuf, InstallError>;

    /// The version of an already-placed `name` in `dir`, if present — the
    /// idempotency probe. `None` when the binary is absent or its version can't
    /// be read (which reads as "not current", i.e. proceed to install).
    fn installed_version(&self, dir: &Path, name: &str) -> Option<String>;

    /// Record the install-time version marker for `name` in `dir` (see
    /// [`super::version_marker_path`]) so a managed release binary reports its
    /// version even though the MCP tool binaries have no `--version`. Written
    /// only after a binary is successfully placed. The default is a no-op —
    /// seams that never place binaries (provider-resolution / docker-only fakes)
    /// need not record anything; [`RealInstallerIo`] writes the real file.
    fn write_version_marker(
        &self,
        _dir: &Path,
        _name: &str,
        _version: &str,
    ) -> Result<(), InstallError> {
        Ok(())
    }

    /// The praxec-managed bin dir binaries are placed on (`<config-dir>/bin`).
    /// On the seam so tests inject a tempdir; the real impl resolves it via
    /// `dirs`, the same convention `init` uses for the config dir.
    fn bin_dir(&self) -> Result<PathBuf, InstallError>;

    /// Is `cmd` an executable on `PATH`? The docker provider's availability
    /// gate — `which("docker")` false means the chain falls through to release
    /// (a fresh machine with no daemon is never blocked). A pure read: it never
    /// mutates, so it is safe to call from `resolve_provider` / offer paths.
    fn which(&self, cmd: &str) -> bool;

    /// `docker pull <image_ref>` (`<image>:<version>`). The mutating half of the
    /// docker provider — invoked only under `Consent::Granted`. Fails loud with
    /// the image ref on a non-zero exit / spawn failure.
    fn docker_pull(&self, image_ref: &str) -> Result<(), InstallError>;
}

/// If `bytes[start..]` begins with a `\d+\.\d+\.\d+` (three dot-separated digit
/// runs), return the end index (exclusive) of that semver core; else `None`.
/// Trailing junk (a pre-release/build suffix, punctuation) is left to the
/// caller — only the `x.y.z` core is matched.
fn semver_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start;
    for part in 0..3 {
        let digits_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == digits_start {
            return None; // a component with no digits
        }
        if part < 2 {
            // components 0 and 1 must be followed by a `.` separator
            if i < bytes.len() && bytes[i] == b'.' {
                i += 1;
            } else {
                return None;
            }
        }
    }
    Some(i)
}

/// Extract the first semver-ish token (`\d+\.\d+\.\d+`, optional `v`/`V`
/// prefix) appearing anywhere in `text` — tolerating `tool 1.2.3`, `tool
/// v0.4.0`, a bare `1.2.3`, or a version mid-line. Returns the bare `x.y.z`
/// (any `v` prefix stripped). `None` when none parses — the SAFE direction
/// (unknown → reinstall, never a wrong "current").
fn parse_version(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    for i in 0..bytes.len() {
        if !bytes[i].is_ascii_digit() {
            continue;
        }
        // Only start a match at a digit-run boundary so we don't begin partway
        // through a longer numeric token (e.g. skip into `12345`); a `v`/`V`
        // prefix is a letter, so it forms a clean boundary and is dropped.
        if i > 0 {
            let prev = bytes[i - 1];
            if prev.is_ascii_digit() || prev == b'.' {
                continue;
            }
        }
        if let Some(end) = semver_end(bytes, i) {
            return Some(String::from_utf8_lossy(&bytes[i..end]).into_owned());
        }
    }
    None
}

/// The production [`InstallerIo`]: the blocking `reqwest` client already in the
/// workspace + real filesystem, and `dirs` for the config-dir convention.
pub struct RealInstallerIo;

impl InstallerIo for RealInstallerIo {
    fn http_get(&self, url: &str) -> Result<Vec<u8>, InstallError> {
        let resp = http_client()?
            .get(url)
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .map_err(|e| InstallError::Io(e.to_string()))?;
        Ok(resp
            .bytes()
            .map_err(|e| InstallError::Io(e.to_string()))?
            .to_vec())
    }

    fn place_executable(
        &self,
        dir: &Path,
        name: &str,
        bytes: &[u8],
    ) -> Result<PathBuf, InstallError> {
        std::fs::create_dir_all(dir).map_err(|e| InstallError::Io(e.to_string()))?;
        // On Windows a spawnable binary needs the `.exe` suffix; the logical
        // `name` (the tool's `command`) stays extension-free elsewhere.
        let file_name = if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.to_string()
        };
        let path = dir.join(file_name);
        std::fs::write(&path, bytes).map_err(|e| InstallError::Io(e.to_string()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path)
                .map_err(|e| InstallError::Io(e.to_string()))?
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).map_err(|e| InstallError::Io(e.to_string()))?;
        }
        Ok(path)
    }

    fn write_version_marker(
        &self,
        dir: &Path,
        name: &str,
        version: &str,
    ) -> Result<(), InstallError> {
        std::fs::create_dir_all(dir).map_err(|e| InstallError::Io(e.to_string()))?;
        let path = super::version_marker_path(dir, name);
        std::fs::write(&path, version).map_err(|e| InstallError::Io(e.to_string()))?;
        Ok(())
    }

    fn installed_version(&self, dir: &Path, name: &str) -> Option<String> {
        // Resolve the placed binary through the ONE `.exe`-aware managed-bin
        // predicate (shared with `detect` + currency), so the existence rule
        // never drifts. Absent → `None` (proceed to reinstall).
        let path = super::managed_binary_in(dir, name)?;
        // The install-time version marker WINS when present: the MCP tool
        // binaries have no `--version` (they ignore the flag and start their
        // stdio server), so probing them yields `None`; the marker recorded at
        // install is the truthful version. A pre-marker binary (older install)
        // falls through to the bounded probe below and self-heals on reinstall.
        let marker = super::version_marker_path(dir, name);
        if let Ok(recorded) = std::fs::read_to_string(&marker) {
            let recorded = recorded.trim();
            if !recorded.is_empty() {
                return Some(recorded.to_string());
            }
        }
        // Best-effort: `<bin> --version` typically prints `name x.y.z`; take the
        // last whitespace token of the first line. Any failure → `None` (proceed
        // to reinstall) rather than a false "current". The probe is BOUNDED
        // (`VERSION_PROBE_TIMEOUT`): a binary that hangs on `--version` is killed
        // and reads as unknown, never hanging `doctor`.
        let mut cmd = std::process::Command::new(&path);
        cmd.arg("--version");
        let out = output_bounded(cmd, VERSION_PROBE_TIMEOUT)?;
        if !out.status.success() {
            return None;
        }
        // Tolerate common `--version` shapes — `tool 1.2.3`, `tool v1.2.3`, a
        // bare `1.2.3`, or a version anywhere on the line — and fall back to
        // stderr (some tools print there). Extract the first semver-ish token;
        // if none parses, `None` (→ reinstall), NEVER a wrong "current".
        let stdout = String::from_utf8_lossy(&out.stdout);
        if let Some(v) = parse_version(&stdout) {
            return Some(v);
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        parse_version(&stderr)
    }

    fn bin_dir(&self) -> Result<PathBuf, InstallError> {
        // Delegate to the ONE definition of the managed bin dir (mod.rs) so the
        // installer and the MCP child-spawn PATH injection never drift.
        super::managed_bin_dir().ok_or_else(|| {
            InstallError::Io(
                "cannot locate a config directory on this machine for the praxec bin dir"
                    .to_string(),
            )
        })
    }

    fn which(&self, cmd: &str) -> bool {
        let Some(paths) = std::env::var_os("PATH") else {
            return false;
        };
        // On Windows an executable may carry a PATHEXT suffix; the empty ext
        // covers an explicit name and every unix binary.
        let exts: &[&str] = if cfg!(windows) {
            &["", ".exe", ".cmd", ".bat"]
        } else {
            &[""]
        };
        std::env::split_paths(&paths).any(|dir| {
            exts.iter()
                .any(|ext| dir.join(format!("{cmd}{ext}")).is_file())
        })
    }

    fn docker_pull(&self, image_ref: &str) -> Result<(), InstallError> {
        let status = std::process::Command::new("docker")
            .arg("pull")
            .arg(image_ref)
            .status()
            .map_err(|e| InstallError::DockerPull {
                image: image_ref.to_string(),
                reason: e.to_string(),
            })?;
        if status.success() {
            Ok(())
        } else {
            Err(InstallError::DockerPull {
                image: image_ref.to_string(),
                reason: format!("`docker pull` exited with {status}"),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The release-download client is built (once) with an explicit timeout —
    /// construction must succeed, offline and without issuing any request. This
    /// pins that the timeout-carrying builder is well-formed; `reqwest` exposes
    /// no getter for the configured timeout, so the bound itself is covered by
    /// inspection of `http_client` (see `HTTP_TIMEOUT`).
    #[test]
    fn http_client_builds_with_an_explicit_timeout() {
        assert!(
            http_client().is_ok(),
            "the timeout-bounded blocking client must build"
        );
    }

    // ── M4: the version-probe parser tolerates common `--version` shapes ──────
    #[test]
    fn parse_version_reads_a_name_prefixed_version() {
        assert_eq!(parse_version("mytool 1.2.3"), Some("1.2.3".to_string()));
    }

    #[test]
    fn parse_version_strips_a_v_prefix() {
        assert_eq!(parse_version("mytool v0.4.0"), Some("0.4.0".to_string()));
    }

    #[test]
    fn parse_version_reads_a_bare_version_line() {
        assert_eq!(parse_version("1.2.3\n"), Some("1.2.3".to_string()));
    }

    #[test]
    fn parse_version_reads_a_version_anywhere_on_the_line() {
        assert_eq!(
            parse_version("mytool version 2.10.0 (build abc)"),
            Some("2.10.0".to_string())
        );
    }

    // ── the bounded `--version` probe: a hang is killed, a fast run collected ─
    #[cfg(unix)]
    #[test]
    fn output_bounded_kills_a_hanging_child_and_returns_none() {
        // A child that never exits within the bound must be killed → None, and
        // the call must return promptly (well under the child's own lifetime),
        // proving `doctor` cannot hang on a stuck `--version`.
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "sleep 10"]);
        let start = Instant::now();
        let out = output_bounded(cmd, Duration::from_millis(200));
        assert!(out.is_none(), "a hanging probe reads as unknown (None)");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "the bounded probe returns promptly, not after the child's 10s"
        );
    }

    #[cfg(unix)]
    #[test]
    fn output_bounded_collects_a_fast_child_output() {
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "echo hi"]);
        let out = output_bounded(cmd, Duration::from_secs(5)).expect("fast child yields output");
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
    }

    #[test]
    fn parse_version_returns_none_for_garbage() {
        // No semver-ish token → None (the SAFE direction: reinstall, never a
        // wrong "current").
        assert_eq!(parse_version("no version here"), None);
        assert_eq!(parse_version("build 42"), None);
        assert_eq!(parse_version(""), None);
    }

    // ── PART B: the install-time marker is read back over an absent `--version` ─
    #[cfg(unix)]
    #[test]
    fn installed_version_reads_the_marker_when_the_binary_has_no_version() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        // A placed "binary" that has NO parseable `--version` (it errors), just
        // like the real MCP tool binaries that ignore the flag.
        let bin = dir.path().join("toolx");
        std::fs::write(&bin, "#!/bin/sh\nexit 3\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        // No marker yet → the errored `--version` probe yields None (fail-safe:
        // unknown → reinstall, never a wrong verdict).
        assert_eq!(RealInstallerIo.installed_version(dir.path(), "toolx"), None);

        // Record the install-time marker → it now reports that version despite
        // the binary having no usable `--version`.
        RealInstallerIo
            .write_version_marker(dir.path(), "toolx", "0.0.7")
            .unwrap();
        assert_eq!(
            RealInstallerIo.installed_version(dir.path(), "toolx"),
            Some("0.0.7".to_string()),
            "the install-time marker wins over an unreadable --version"
        );
    }

    #[cfg(unix)]
    #[test]
    fn installed_version_is_none_when_the_binary_is_absent_even_with_a_marker() {
        // A marker without a placed binary is not "installed" — existence is
        // resolved through the ONE managed-bin predicate first, so a stray marker
        // never fabricates a currency verdict.
        let dir = tempfile::tempdir().unwrap();
        RealInstallerIo
            .write_version_marker(dir.path(), "ghost", "9.9.9")
            .unwrap();
        assert_eq!(RealInstallerIo.installed_version(dir.path(), "ghost"), None);
    }
}
