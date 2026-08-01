//! The [`InstallerIo`] seam — every host-touching operation the release
//! provider needs, injectable so the resolve → verify → place decision logic in
//! [`super::install_release`] is unit-tested with a fake, never a real network
//! or filesystem. Mirrors the [`crate::currency::CurrencyIo`] idiom (a trait +
//! a `Real*` production impl).

use std::path::{Path, PathBuf};

use super::InstallError;

/// The host-touching operations the release provider needs. Kept minimal: an
/// HTTP GET (for the asset + its `checksums.sha256`), an executable placement,
/// an installed-version probe (idempotency), and the praxec-managed bin dir.
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
}

/// The production [`InstallerIo`]: the blocking `reqwest` client already in the
/// workspace + real filesystem, and `dirs` for the config-dir convention.
pub struct RealInstallerIo;

impl InstallerIo for RealInstallerIo {
    fn http_get(&self, url: &str) -> Result<Vec<u8>, InstallError> {
        let resp = reqwest::blocking::get(url)
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
        dirs::config_dir()
            .map(|d| d.join("praxec").join("bin"))
            .ok_or_else(|| {
                InstallError::Io(
                    "cannot locate a config directory on this machine for the praxec bin dir"
                        .to_string(),
                )
            })
    }
}
