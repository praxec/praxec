//! `ProvisionInstaller` — the release provider: get a *prebuilt* tool binary
//! onto the machine, integrity-verified, with zero compilation.
//!
//! This is the core "obtain a tool binary" mechanism of the onboarding path
//! (`docs/design/2026-08-01-onboarding-tool-provisioning.md` §4). It consumes a
//! [`RegistryTool`](crate::registry_v3::RegistryTool) from the loaded registry
//! plus the host `(os, arch)` and, behind the [`InstallerIo`] seam:
//!
//! 1. resolves `(os, arch)` → the release **target triple the tools publish**
//!    and the asset name (uniform convention `{command}-{triple}.{ext}` —
//!    *derived*, because the convention is itself the data: §3 principle 5);
//! 2. downloads the asset + its `checksums.sha256` from the tool's `release`
//!    provider page (`<page>/download/v{version}/<asset>`);
//! 3. **verifies the asset bytes against the sha256 in `checksums.sha256`;
//!    refuses on mismatch and never places the binary** (§4, FMECA "tampered/
//!    corrupt binary" → Low);
//! 4. unpacks (`.tar.gz` unix / `.zip` windows), extracts the `command` binary,
//!    and places it on the praxec-managed bin dir via [`InstallerIo`];
//! 5. is idempotent (an already-current binary → [`InstallOutcome::AlreadyCurrent`]
//!    with no download or write) and fails fast with the resolved URL + triple
//!    on any 404 / mismatch / unpack failure.
//!
//! CRITICAL: the tools publish against `x86_64-unknown-linux-gnu` (the GNU
//! triple, *not* praxec's own musl) — see [`resolve_target`].
//!
//! The docker provider and the Release → Docker → Cargo provider chain are
//! Task 3 (not here); this module is the release provider + the seam only.

use std::io::Read;
use std::path::PathBuf;

use sha2::{Digest, Sha256};

use crate::registry_v3::RegistryTool;
use crate::tool_descriptor::ProvisionProvider;

pub mod io;
pub use io::{InstallerIo, RealInstallerIo};

/// The host an install targets. `os` matches `std::env::consts::OS`
/// (`"linux"`, `"macos"`, `"windows"`) — `"darwin"` is accepted as an alias —
/// and `arch` matches `std::env::consts::ARCH` (`"x86_64"`, `"aarch64"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    pub os: String,
    pub arch: String,
}

/// The outcome of a release install. `Refused` is a *decision* (integrity
/// check failed) distinct from an [`InstallError`] (an operational failure);
/// both leave the machine unmutated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallOutcome {
    /// A fresh binary was verified and placed.
    Installed { path: PathBuf, version: String },
    /// The requested version was already placed — no download, no write.
    AlreadyCurrent,
    /// Integrity refused the asset (checksum mismatch) — nothing was placed.
    Refused { reason: String },
}

/// Typed failures from the release provider. Every message carries a stable
/// `SCREAMING_SNAKE` code + the resolved URL / host triple so an operator (or
/// audit reader) can diagnose without string archaeology — mirrors
/// [`crate::registry_v3::RegistryError`].
#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    /// No release-asset mapping for this host — praxec never guesses a triple.
    #[error("INSTALL_UNSUPPORTED_HOST: no release asset mapping for (os `{os}`, arch `{arch}`)")]
    UnsupportedHost { os: String, arch: String },
    /// The tool lacks a field the release provider requires (`command`,
    /// `version`, or a `release` provider URL).
    #[error("INSTALL_MISSING_FIELD: tool `{tool}` release provider requires `{field}`")]
    MissingField { tool: String, field: &'static str },
    /// Fetching a URL failed (404 / unreachable / transport). Names the URL and
    /// the `(os, arch)` triple so a wrong-asset resolution is self-evident.
    #[error("INSTALL_DOWNLOAD: fetching `{url}` for (os `{os}`, arch `{arch}`) failed: {reason}")]
    Download {
        url: String,
        os: String,
        arch: String,
        reason: String,
    },
    /// `checksums.sha256` was fetched but carries no line for the resolved
    /// asset — integrity cannot be established, so the install refuses to place.
    #[error("INSTALL_CHECKSUM_ABSENT: `{url}` has no `checksums.sha256` entry for asset `{asset}`")]
    ChecksumAbsent { url: String, asset: String },
    /// The asset downloaded + verified but could not be unpacked / the expected
    /// `command` binary was not inside it.
    #[error("INSTALL_UNPACK: unpacking `{asset}` (from `{url}`) failed: {reason}")]
    Unpack {
        url: String,
        asset: String,
        reason: String,
    },
    /// A filesystem / placement failure from the [`InstallerIo`] impl.
    #[error("INSTALL_IO: {0}")]
    Io(String),
}

/// Resolve a host `(os, arch)` to the `(target-triple, archive-ext)` the
/// `/praxec/*` tools publish. CRITICAL: linux uses the **GNU** triple
/// (`x86_64-unknown-linux-gnu`), not praxec's own musl. Returns `None` for a
/// host the tools do not publish for (fail fast, never guess).
pub fn resolve_target(os: &str, arch: &str) -> Option<(&'static str, &'static str)> {
    match (os, arch) {
        ("linux", "x86_64") => Some(("x86_64-unknown-linux-gnu", "tar.gz")),
        ("linux", "aarch64") => Some(("aarch64-unknown-linux-gnu", "tar.gz")),
        ("macos" | "darwin", "x86_64") => Some(("x86_64-apple-darwin", "tar.gz")),
        ("macos" | "darwin", "aarch64") => Some(("aarch64-apple-darwin", "tar.gz")),
        ("windows", "x86_64") => Some(("x86_64-pc-windows-msvc", "zip")),
        _ => None,
    }
}

/// The release asset name for a tool `command` on a `(triple, ext)` — the
/// uniform `{command}-{triple}.{ext}` convention (§3 principle 5: the
/// convention *is* the data; no per-tool registry field).
pub fn asset_name(command: &str, triple: &str, ext: &str) -> String {
    format!("{command}-{triple}.{ext}")
}

/// Lowercase hex of the sha256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut s = String::with_capacity(digest.len() * 2);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Find the expected sha256 for `asset` in a `checksums.sha256` file. Each line
/// is `<hex>  <name>` (sha256sum format); the name may carry a `*` (binary
/// mode) or `./` prefix. Matches on the file's basename. Pure.
pub fn expected_sha256<'a>(checksums: &'a str, asset: &str) -> Option<&'a str> {
    for line in checksums.lines() {
        let mut toks = line.split_whitespace();
        let hash = toks.next()?;
        let Some(name) = toks.next() else { continue };
        let name = name.trim_start_matches('*').trim_start_matches("./");
        let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
        if base == asset {
            return Some(hash);
        }
    }
    None
}

/// Extract the `command` executable's bytes from a downloaded archive. `.tar.gz`
/// is gunzipped + untarred; `.zip` is read directly. The entry matched is the
/// one whose basename is `command` or `{command}.exe`. Returns the executable
/// bytes, or a human error for the [`InstallError::Unpack`] wrapper.
fn unpack_command(ext: &str, bytes: &[u8], command: &str) -> Result<Vec<u8>, String> {
    let want = command;
    let want_exe = format!("{command}.exe");
    let matches = |name: &str| {
        let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
        base == want || base == want_exe
    };
    match ext {
        "tar.gz" => {
            let gz = flate2::read::GzDecoder::new(bytes);
            let mut ar = tar::Archive::new(gz);
            let entries = ar.entries().map_err(|e| e.to_string())?;
            for entry in entries {
                let mut entry = entry.map_err(|e| e.to_string())?;
                let path = entry.path().map_err(|e| e.to_string())?;
                let name = path.to_string_lossy().to_string();
                if matches(&name) {
                    let mut buf = Vec::new();
                    entry.read_to_end(&mut buf).map_err(|e| e.to_string())?;
                    return Ok(buf);
                }
            }
            Err(format!(
                "no entry named `{command}` (or `{command}.exe`) in the tar.gz"
            ))
        }
        "zip" => {
            let reader = std::io::Cursor::new(bytes);
            let mut zip = zip::ZipArchive::new(reader).map_err(|e| e.to_string())?;
            for i in 0..zip.len() {
                let mut file = zip.by_index(i).map_err(|e| e.to_string())?;
                let name = file.name().to_string();
                if matches(&name) {
                    let mut buf = Vec::new();
                    file.read_to_end(&mut buf).map_err(|e| e.to_string())?;
                    return Ok(buf);
                }
            }
            Err(format!(
                "no entry named `{command}` (or `{command}.exe`) in the zip"
            ))
        }
        other => Err(format!("unsupported archive extension `{other}`")),
    }
}

/// Install a tool from its **release** provider: resolve → download → verify →
/// unpack → place, idempotent and fail-fast. See the module docs for the full
/// contract. Does the docker chain NOT belong here — that is Task 3.
pub fn install_release(
    tool: &RegistryTool,
    host: &Host,
    io: &dyn InstallerIo,
) -> Result<InstallOutcome, InstallError> {
    // Required release-provider coordinates — fail fast, never guess.
    let command = tool
        .command
        .as_deref()
        .ok_or_else(|| InstallError::MissingField {
            tool: tool.id.clone(),
            field: "command",
        })?;
    let version = tool
        .version
        .as_deref()
        .ok_or_else(|| InstallError::MissingField {
            tool: tool.id.clone(),
            field: "version",
        })?;
    let page = tool
        .providers
        .get(ProvisionProvider::Release.as_token())
        .ok_or_else(|| InstallError::MissingField {
            tool: tool.id.clone(),
            field: "providers.release",
        })?;

    let bin_dir = io.bin_dir()?;

    // Idempotency: an already-current binary short-circuits before any network.
    if io.installed_version(&bin_dir, command).as_deref() == Some(version) {
        return Ok(InstallOutcome::AlreadyCurrent);
    }

    // Resolve the published triple + asset (or fail fast for this host).
    let (triple, ext) =
        resolve_target(&host.os, &host.arch).ok_or_else(|| InstallError::UnsupportedHost {
            os: host.os.clone(),
            arch: host.arch.clone(),
        })?;
    let asset = asset_name(command, triple, ext);

    let page = page.trim_end_matches('/');
    let asset_url = format!("{page}/download/v{version}/{asset}");
    let checksums_url = format!("{page}/download/v{version}/checksums.sha256");

    // Download the asset — a 404 / unreachable becomes a Download error naming
    // the resolved URL + host triple regardless of the IO impl's own message.
    let asset_bytes = io
        .http_get(&asset_url)
        .map_err(|e| InstallError::Download {
            url: asset_url.clone(),
            os: host.os.clone(),
            arch: host.arch.clone(),
            reason: e.to_string(),
        })?;
    let checksums_bytes = io
        .http_get(&checksums_url)
        .map_err(|e| InstallError::Download {
            url: checksums_url.clone(),
            os: host.os.clone(),
            arch: host.arch.clone(),
            reason: e.to_string(),
        })?;

    // Integrity: no entry → cannot verify → refuse (error); mismatch → refuse
    // (outcome). In NEITHER case is a byte written.
    let checksums = String::from_utf8_lossy(&checksums_bytes);
    let expected =
        expected_sha256(&checksums, &asset).ok_or_else(|| InstallError::ChecksumAbsent {
            url: checksums_url.clone(),
            asset: asset.clone(),
        })?;
    let actual = sha256_hex(&asset_bytes);
    if !actual.eq_ignore_ascii_case(expected) {
        return Ok(InstallOutcome::Refused {
            reason: format!(
                "checksum mismatch for `{asset}` from {asset_url}: expected {expected}, got {actual}"
            ),
        });
    }

    // Verified — unpack + place.
    let exe_bytes =
        unpack_command(ext, &asset_bytes, command).map_err(|reason| InstallError::Unpack {
            url: asset_url.clone(),
            asset: asset.clone(),
            reason,
        })?;
    let path = io.place_executable(&bin_dir, command, &exe_bytes)?;
    Ok(InstallOutcome::Installed {
        path,
        version: version.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::io::Write;
    use std::path::Path;

    // ── fake IO: everything is an in-memory map; writes are recorded ─────────

    #[derive(Default)]
    struct FakeIo {
        /// url → bytes (a missing url is a 404-equivalent).
        responses: HashMap<String, Vec<u8>>,
        /// what `installed_version` should report for `(dir, name)`.
        installed: Option<String>,
        bin_dir: PathBuf,
        /// recorded executable placements: (dir, name, byte-len).
        writes: RefCell<Vec<(PathBuf, String, usize)>>,
    }

    impl InstallerIo for FakeIo {
        fn http_get(&self, url: &str) -> Result<Vec<u8>, InstallError> {
            self.responses
                .get(url)
                .cloned()
                .ok_or_else(|| InstallError::Io(format!("404 {url}")))
        }
        fn place_executable(
            &self,
            dir: &Path,
            name: &str,
            bytes: &[u8],
        ) -> Result<PathBuf, InstallError> {
            self.writes
                .borrow_mut()
                .push((dir.to_path_buf(), name.to_string(), bytes.len()));
            Ok(dir.join(name))
        }
        fn installed_version(&self, _dir: &Path, _name: &str) -> Option<String> {
            self.installed.clone()
        }
        fn bin_dir(&self) -> Result<PathBuf, InstallError> {
            Ok(self.bin_dir.clone())
        }
    }

    // ── fixture builders ─────────────────────────────────────────────────────

    fn tool(command: &str, version: &str, page: &str) -> RegistryTool {
        RegistryTool {
            id: format!("{command}-tool"),
            name: command.to_string(),
            description: String::new(),
            repo: None,
            command: Some(command.to_string()),
            version: Some(version.to_string()),
            mcp_registry_id: None,
            providers: [("release".to_string(), page.to_string())]
                .into_iter()
                .collect(),
            descriptor: None,
            suggested_workflows: Vec::new(),
        }
    }

    fn make_targz(entry_name: &str, contents: &[u8]) -> Vec<u8> {
        let enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(enc);
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, entry_name, contents)
            .unwrap();
        let enc = builder.into_inner().unwrap();
        enc.finish().unwrap()
    }

    fn make_zip(entry_name: &str, contents: &[u8]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            writer.start_file(entry_name, opts).unwrap();
            writer.write_all(contents).unwrap();
            writer.finish().unwrap();
        }
        buf.into_inner()
    }

    fn checksums_line(asset: &str, bytes: &[u8]) -> String {
        format!("{}  {asset}\n", sha256_hex(bytes))
    }

    // ── pure-unit contracts (one assertion each) ─────────────────────────────

    #[test]
    fn linux_x86_64_resolves_to_the_gnu_triple_not_musl() {
        assert_eq!(
            resolve_target("linux", "x86_64"),
            Some(("x86_64-unknown-linux-gnu", "tar.gz"))
        );
    }

    #[test]
    fn windows_x86_64_resolves_to_a_zip_asset() {
        assert_eq!(
            resolve_target("windows", "x86_64"),
            Some(("x86_64-pc-windows-msvc", "zip"))
        );
    }

    #[test]
    fn asset_name_is_the_uniform_command_triple_ext_convention() {
        assert_eq!(
            asset_name("cpm-planner", "x86_64-apple-darwin", "tar.gz"),
            "cpm-planner-x86_64-apple-darwin.tar.gz"
        );
    }

    #[test]
    fn expected_sha256_matches_the_asset_line_ignoring_star_and_dir() {
        let file = "deadbeef  *./cpm-planner-x86_64-unknown-linux-gnu.tar.gz\n\
                    cafef00d  other-asset.zip\n";
        assert_eq!(
            expected_sha256(file, "cpm-planner-x86_64-unknown-linux-gnu.tar.gz"),
            Some("deadbeef")
        );
    }

    // ── install_release contracts ────────────────────────────────────────────

    #[test]
    fn good_asset_and_checksum_install_the_binary() {
        let bin = PathBuf::from("/fake/bin");
        let page = "https://github.com/praxec/cpm-planner/releases";
        let host = Host {
            os: "linux".into(),
            arch: "x86_64".into(),
        };
        let asset = "cpm-planner-x86_64-unknown-linux-gnu.tar.gz";
        let asset_bytes = make_targz("cpm-planner", b"#!/bin/sh\necho hi\n");
        let mut responses = HashMap::new();
        responses.insert(
            format!("{page}/download/v0.0.2/{asset}"),
            asset_bytes.clone(),
        );
        responses.insert(
            format!("{page}/download/v0.0.2/checksums.sha256"),
            checksums_line(asset, &asset_bytes).into_bytes(),
        );
        let io = FakeIo {
            responses,
            bin_dir: bin.clone(),
            ..Default::default()
        };

        let out = install_release(&tool("cpm-planner", "0.0.2", page), &host, &io).unwrap();
        assert_eq!(
            out,
            InstallOutcome::Installed {
                path: bin.join("cpm-planner"),
                version: "0.0.2".into()
            }
        );
        assert_eq!(io.writes.borrow().len(), 1, "exactly one executable placed");
    }

    #[test]
    fn bad_checksum_refuses_and_writes_nothing() {
        let page = "https://github.com/praxec/cpm-planner/releases";
        let host = Host {
            os: "linux".into(),
            arch: "x86_64".into(),
        };
        let asset = "cpm-planner-x86_64-unknown-linux-gnu.tar.gz";
        let asset_bytes = make_targz("cpm-planner", b"real bytes");
        let mut responses = HashMap::new();
        responses.insert(format!("{page}/download/v0.0.2/{asset}"), asset_bytes);
        // A checksum for DIFFERENT bytes → mismatch.
        responses.insert(
            format!("{page}/download/v0.0.2/checksums.sha256"),
            checksums_line(asset, b"tampered").into_bytes(),
        );
        let io = FakeIo {
            responses,
            bin_dir: "/fake/bin".into(),
            ..Default::default()
        };

        let out = install_release(&tool("cpm-planner", "0.0.2", page), &host, &io).unwrap();
        assert!(matches!(out, InstallOutcome::Refused { .. }), "got {out:?}");
        assert_eq!(
            io.writes.borrow().len(),
            0,
            "no binary placed on checksum mismatch"
        );
    }

    #[test]
    fn already_current_version_is_a_no_op_without_download_or_write() {
        let host = Host {
            os: "linux".into(),
            arch: "x86_64".into(),
        };
        // No responses registered — any http_get would 404; a download proves
        // the idempotency short-circuit failed.
        let io = FakeIo {
            installed: Some("0.0.2".into()),
            bin_dir: "/fake/bin".into(),
            ..Default::default()
        };
        let out = install_release(
            &tool("cpm-planner", "0.0.2", "https://example.com/releases"),
            &host,
            &io,
        )
        .unwrap();
        assert_eq!(out, InstallOutcome::AlreadyCurrent);
        assert_eq!(io.writes.borrow().len(), 0);
    }

    #[test]
    fn missing_asset_fails_fast_with_the_url_and_triple() {
        let page = "https://github.com/praxec/cpm-planner/releases";
        let host = Host {
            os: "linux".into(),
            arch: "x86_64".into(),
        };
        // No responses → the asset GET 404s.
        let io = FakeIo {
            bin_dir: "/fake/bin".into(),
            ..Default::default()
        };
        let err = install_release(&tool("cpm-planner", "0.0.2", page), &host, &io).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("cpm-planner-x86_64-unknown-linux-gnu.tar.gz"),
            "message names the resolved asset URL: {msg}"
        );
        assert!(
            msg.contains("x86_64"),
            "message names the host triple: {msg}"
        );
    }

    #[test]
    fn unsupported_host_fails_fast_naming_os_and_arch() {
        let host = Host {
            os: "plan9".into(),
            arch: "sparc".into(),
        };
        let io = FakeIo {
            bin_dir: "/fake/bin".into(),
            ..Default::default()
        };
        let err = install_release(
            &tool("cpm-planner", "0.0.2", "https://example.com/releases"),
            &host,
            &io,
        )
        .unwrap_err();
        assert!(matches!(err, InstallError::UnsupportedHost { .. }));
        assert!(err.to_string().contains("plan9"));
    }

    #[test]
    fn tar_gz_unix_asset_unpacks_and_installs() {
        let page = "https://p/releases";
        let host = Host {
            os: "macos".into(),
            arch: "aarch64".into(),
        };
        let asset = "widget-aarch64-apple-darwin.tar.gz";
        let asset_bytes = make_targz("widget", b"mach-o bytes");
        let mut responses = HashMap::new();
        responses.insert(
            format!("{page}/download/v1.0.0/{asset}"),
            asset_bytes.clone(),
        );
        responses.insert(
            format!("{page}/download/v1.0.0/checksums.sha256"),
            checksums_line(asset, &asset_bytes).into_bytes(),
        );
        let io = FakeIo {
            responses,
            bin_dir: "/bin".into(),
            ..Default::default()
        };
        let out = install_release(&tool("widget", "1.0.0", page), &host, &io).unwrap();
        assert!(matches!(out, InstallOutcome::Installed { .. }));
    }

    #[test]
    fn zip_windows_asset_unpacks_the_exe_and_installs() {
        let page = "https://p/releases";
        let host = Host {
            os: "windows".into(),
            arch: "x86_64".into(),
        };
        let asset = "widget-x86_64-pc-windows-msvc.zip";
        // Windows assets carry `widget.exe` inside — exercises the `.exe` match.
        let asset_bytes = make_zip("widget.exe", b"MZ windows bytes");
        let mut responses = HashMap::new();
        responses.insert(
            format!("{page}/download/v1.0.0/{asset}"),
            asset_bytes.clone(),
        );
        responses.insert(
            format!("{page}/download/v1.0.0/checksums.sha256"),
            checksums_line(asset, &asset_bytes).into_bytes(),
        );
        let io = FakeIo {
            responses,
            bin_dir: "/bin".into(),
            ..Default::default()
        };
        let out = install_release(&tool("widget", "1.0.0", page), &host, &io).unwrap();
        assert!(matches!(out, InstallOutcome::Installed { .. }));
        assert_eq!(
            io.writes.borrow()[0].1,
            "widget",
            "placed under the logical command name"
        );
    }
}
