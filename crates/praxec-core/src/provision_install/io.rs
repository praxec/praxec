//! The [`InstallerIo`] seam — every host-touching operation the release
//! provider needs, injectable so the resolve → verify → place decision logic in
//! [`super::install_release`] is unit-tested with a fake, never a real network
//! or filesystem. Mirrors the [`crate::currency::CurrencyIo`] idiom (a trait +
//! a `Real*` production impl).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use super::InstallError;

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

    fn installed_version(&self, dir: &Path, name: &str) -> Option<String> {
        let file_name = if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.to_string()
        };
        let path = dir.join(file_name);
        if !path.exists() {
            return None;
        }
        // Best-effort: `<bin> --version` typically prints `name x.y.z`; take the
        // last whitespace token of the first line. Any failure → `None` (proceed
        // to reinstall) rather than a false "current".
        let out = std::process::Command::new(&path)
            .arg("--version")
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let line = String::from_utf8_lossy(&out.stdout);
        let first = line.lines().next()?.trim();
        first.split_whitespace().last().map(str::to_string)
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
}
