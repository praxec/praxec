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
//! 2. downloads the asset from the tool's `release` provider page
//!    (`<page>/download/v{version}/<asset>`) and resolves its expected sha256
//!    from EITHER checksum convention — the aggregate `checksums.sha256`
//!    (praxec's own) OR, on its 404, the per-asset `<asset>.sha256` sidecar
//!    (`taiki-e/upload-rust-binary-action` with `checksum: sha256`);
//! 3. **verifies the asset bytes against that sha256; refuses on mismatch and
//!    fails-CLOSED (never places the binary) when neither convention yields a
//!    usable hash** (§4, FMECA "tampered/corrupt binary" → Low);
//! 4. unpacks (`.tar.gz` unix / `.zip` windows), extracts the `command` binary,
//!    places it on the praxec-managed bin dir via [`InstallerIo`], and records
//!    an install-time version marker ([`version_marker_path`]) so currency can
//!    read the version back even though the MCP tool binaries have no
//!    `--version`;
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
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::registry_v3::RegistryTool;
use crate::tool_descriptor::ProvisionProvider;

pub mod io;
pub use io::{InstallerIo, RealInstallerIo};

pub mod provider;
pub use provider::{Consent, InstallPlan, Provider, install, resolve_provider};

pub mod from_candidate;
pub use from_candidate::{InstallTarget, from_candidate};

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
    /// A non-mutating *plan* only: the provider the chain would use and the
    /// exact human-readable command, produced by [`Consent::OfferOnly`] (and by
    /// the emit-only cargo arm under [`Consent::Granted`]). Nothing was
    /// downloaded, pulled, or written — this is doctor's "offer".
    Offered {
        provider: provider::Provider,
        command: String,
    },
    /// The requested target is a **remote** tool (a discovered
    /// [`ToolSource::Url`](crate::tool_catalog::ToolSource::Url)) — there is
    /// nothing to fetch or place; it is wired as a url connection instead. A
    /// first-class, non-error outcome (§3 principle 1: discovery normalizes when
    /// acted on, and a remote endpoint's normalization is "no install needed").
    NoInstallNeeded { reason: String },
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
    /// Fetching a URL failed (unreachable / transport / non-404 status). Names
    /// the URL and the `(os, arch)` triple so a wrong-asset resolution is
    /// self-evident. A **404** is the distinct [`NotFound`](Self::NotFound) —
    /// it lets checksum resolution fall through one convention to the next
    /// rather than aborting.
    #[error("INSTALL_DOWNLOAD: fetching `{url}` for (os `{os}`, arch `{arch}`) failed: {reason}")]
    Download {
        url: String,
        os: String,
        arch: String,
        reason: String,
    },
    /// A URL returned **404 Not Found** — distinct from [`Download`](Self::Download)
    /// (transport / other non-2xx) so [`http_get`](InstallerIo::http_get) can
    /// signal "absent" and checksum resolution can fall through from the
    /// aggregate `checksums.sha256` convention to the per-asset sidecar
    /// convention instead of aborting.
    #[error("INSTALL_NOT_FOUND: `{url}` returned 404")]
    NotFound { url: String },
    /// `checksums.sha256` was fetched but carries no line for the resolved
    /// asset — integrity cannot be established, so the install refuses to place.
    #[error("INSTALL_CHECKSUM_ABSENT: `{url}` has no `checksums.sha256` entry for asset `{asset}`")]
    ChecksumAbsent { url: String, asset: String },
    /// `checksums.sha256` has a line for the asset, but its hash token is not a
    /// well-formed sha256 (64 hex chars) — a malformed/truncated checksum cannot
    /// verify anything, so the install refuses to place (fail-CLOSED). Distinct
    /// from [`ChecksumAbsent`](Self::ChecksumAbsent) (no entry at all).
    #[error(
        "INSTALL_CHECKSUM_MALFORMED: `{url}` entry for asset `{asset}` has a malformed sha256 token `{token}` (expected 64 hex chars)"
    )]
    ChecksumMalformed {
        url: String,
        asset: String,
        token: String,
    },
    /// Neither checksum convention published a hash for the asset: both the
    /// aggregate `checksums.sha256` AND the per-asset `<asset>.sha256` sidecar
    /// 404'd. Integrity cannot be established from either, so the install
    /// refuses to place (fail-CLOSED — an unverified binary is NEVER placed).
    #[error(
        "INSTALL_CHECKSUM_UNAVAILABLE: no checksum for asset `{asset}` — neither the aggregate `{aggregate_url}` nor the per-asset sidecar `{sidecar_url}` exists (both 404)"
    )]
    ChecksumUnavailable {
        aggregate_url: String,
        sidecar_url: String,
        asset: String,
    },
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
    /// `docker pull` failed (spawn error or non-zero exit) — names the image.
    #[error("INSTALL_DOCKER_PULL: `docker pull {image}` failed: {reason}")]
    DockerPull { image: String, reason: String },
    /// The provider chain (release → docker → npx → cargo) resolved no available
    /// provider for this tool + host — fail fast naming the tool.
    #[error(
        "INSTALL_NO_PROVIDER: no available provider (release/docker/npx/cargo) for tool `{tool}`"
    )]
    NoProvider { tool: String },
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

/// The praxec-managed bin directory (`<config-dir>/praxec/bin`) that
/// [`install_release`] places tool binaries into — the ONE definition of this
/// path. Both [`io::RealInstallerIo::bin_dir`] and the MCP child-spawn PATH
/// injection resolve it here, so there is a single source of truth. `None` when
/// the host has no config directory (the same `dirs` convention `init` uses).
pub fn managed_bin_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("praxec").join("bin"))
}

/// The directories PREPENDED to a spawned MCP child's `PATH` so a bare
/// `command:` connection resolves even when the gateway was launched by a GUI or
/// service that did not inherit the interactive shell's `PATH` — the reason
/// operators otherwise hardcode an absolute `~/.cargo/bin/...` command, which is
/// machine- and OS-specific and breaks on every other host. In precedence order:
/// the praxec-managed bin dir ([`managed_bin_dir`]), then the two conventional
/// user tool-install dirs `~/.cargo/bin` and `~/.local/bin` (on Windows these
/// resolve under `%USERPROFILE%` via the `home_dir` join). Existence is NOT
/// required — a missing dir is simply an inert `PATH` entry. Kept here beside
/// [`managed_bin_dir`] so the installer and the child-spawn PATH never drift.
pub fn spawn_path_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(m) = managed_bin_dir() {
        dirs.push(m);
    }
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".cargo").join("bin"));
        dirs.push(home.join(".local").join("bin"));
    }
    dirs
}

/// Does `command` resolve to a spawnable binary the gateway could actually
/// launch? An explicit path (absolute, or containing a separator) is checked
/// directly; a bare name is searched on the ambient `PATH` and in the
/// [`spawn_path_dirs`] praxec prepends, `.exe`-aware on Windows via
/// [`managed_binary_in`]. This mirrors the resolution a real stdio spawn
/// performs, so an `optional`-connection SKIP decision agrees with what a spawn
/// would find (an absent binary → skip; an installed one → spawn). Reuses the
/// ONE `.exe`-aware predicate so install detection and this never drift.
pub fn command_resolves(command: &str) -> bool {
    // Explicit path: the operator named the exact file — check it directly
    // (the machine-specific `~/.cargo/bin/...` case an operator hardcodes).
    if command.contains('/') || command.contains('\\') {
        return Path::new(command).is_file();
    }
    // Bare name: on the ambient PATH, or in a dir praxec prepends to the child
    // PATH (managed bin, ~/.cargo/bin, ~/.local/bin).
    let ambient: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    ambient
        .iter()
        .chain(spawn_path_dirs().iter())
        .any(|d| managed_binary_in(d, command).is_some())
}

/// Resolve a spawnable binary named `command` inside `dir`, `.exe`-aware on
/// Windows: returns the existing file's path, or `None` when absent. This is the
/// ONE managed-bin-dir existence predicate — `provision::detect`, the currency
/// managed-dir probe ([`crate::currency`]-side `RealCurrencyIo`), and the
/// installer's own version probe ([`io::RealInstallerIo::installed_version`]) all
/// resolve a managed binary THROUGH here, so the bare-name / `.exe` rule cannot
/// drift between them. `dir` is a parameter (not [`managed_bin_dir`]) so callers
/// with an injected test dir share the exact same predicate.
pub fn managed_binary_in(dir: &Path, command: &str) -> Option<PathBuf> {
    let bare = dir.join(command);
    if bare.is_file() {
        return Some(bare);
    }
    // On Windows a spawnable binary carries the `.exe` suffix `place_executable`
    // writes; the bare name still wins if present.
    if cfg!(windows) {
        let exe = dir.join(format!("{command}.exe"));
        if exe.is_file() {
            return Some(exe);
        }
    }
    None
}

/// As [`managed_binary_in`] but against the real managed bin dir
/// (`<config-dir>/praxec/bin`). `None` when the host has no config directory or
/// no such binary is placed. The convenience wrapper for callers that resolve
/// against the real dir rather than an injected one.
pub fn managed_binary_path(command: &str) -> Option<PathBuf> {
    managed_binary_in(&managed_bin_dir()?, command)
}

/// The release asset name for a tool `command` on a `(triple, ext)` — the
/// uniform `{command}-{triple}.{ext}` convention (§3 principle 5: the
/// convention *is* the data; no per-tool registry field).
pub fn asset_name(command: &str, triple: &str, ext: &str) -> String {
    format!("{command}-{triple}.{ext}")
}

/// The install-time version marker for `command` inside `dir`:
/// `<dir>/.<command>.version`, holding the exact version string recorded when
/// [`install_release`] placed the binary. Dot-prefixed AND `.version`-suffixed
/// so it can never collide with the spawnable binary (`<command>` or
/// `<command>.exe`) that lives in the same dir. It exists because the MCP tool
/// binaries do not implement `--version` (they ignore the flag and start their
/// stdio server), so the `--version` probe returns `None`; reading this marker
/// back gives a managed release binary a truthful installed version for both
/// install idempotency and `doctor` currency. The ONE definition of the marker
/// name — [`io::RealInstallerIo`] writes and reads it here so the two never drift.
pub fn version_marker_path(dir: &Path, command: &str) -> PathBuf {
    dir.join(format!(".{command}.version"))
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

/// The result of looking an asset up in a `checksums.sha256` file: either a
/// well-formed hash, a matched-but-malformed hash token (fail-CLOSED — never
/// verify against garbage), or no entry at all. Distinguishing the middle case
/// is M3 (a truncated/garbage checksum must not read as "absent").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChecksumLookup<'a> {
    /// A matched line with a well-formed sha256 (64 hex chars).
    Found(&'a str),
    /// A matched line whose hash token is NOT a well-formed sha256.
    Malformed(&'a str),
    /// No line matched the asset.
    Absent,
}

/// Is `token` a well-formed sha256 — exactly 64 ASCII hex digits?
fn is_sha256_token(token: &str) -> bool {
    token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Find the expected sha256 for `asset` in a `checksums.sha256` file. Each line
/// is `<hex>  <name>` (sha256sum format); the name may carry a `*` (binary
/// mode) or `./` prefix. Matches on the file's basename. Pure.
///
/// The matched hash token is shape-validated (M3): a malformed token yields
/// [`ChecksumLookup::Malformed`] (not silently treated as a match), so the
/// caller fails-CLOSED with a distinct diagnostic instead of ever verifying
/// against a truncated/garbage checksum.
pub fn expected_sha256<'a>(checksums: &'a str, asset: &str) -> ChecksumLookup<'a> {
    for line in checksums.lines() {
        let mut toks = line.split_whitespace();
        let Some(hash) = toks.next() else { continue };
        let Some(name) = toks.next() else { continue };
        let name = name.trim_start_matches('*').trim_start_matches("./");
        let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
        if base == asset {
            return if is_sha256_token(hash) {
                ChecksumLookup::Found(hash)
            } else {
                ChecksumLookup::Malformed(hash)
            };
        }
    }
    ChecksumLookup::Absent
}

/// Extract the sha256 from a **per-asset sidecar** (`<asset>.sha256`, the shape
/// `taiki-e/upload-rust-binary-action` emits with `checksum: sha256`). The
/// content is either a bare `<hex>` or sha256sum's `<hex>  <asset>` — in both
/// the hash is the first whitespace token. Pure; never [`ChecksumLookup::Absent`]
/// (the sidecar file IS the asset's line, so an empty / hash-less body is
/// [`ChecksumLookup::Malformed`], fail-CLOSED — never verify against nothing).
pub fn sidecar_sha256(sidecar: &str) -> ChecksumLookup<'_> {
    match sidecar.split_whitespace().next() {
        Some(tok) if is_sha256_token(tok) => ChecksumLookup::Found(tok),
        Some(tok) => ChecksumLookup::Malformed(tok),
        None => ChecksumLookup::Malformed(""),
    }
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

/// Resolve the expected sha256 (lowercase hex) for `asset`, tolerating BOTH
/// checksum-publishing conventions, in order:
///   1. the aggregate `<page>/download/v{v}/checksums.sha256` (praxec's own
///      convention) parsed via [`expected_sha256`];
///   2. the per-asset `<page>/download/v{v}/<asset>.sha256` sidecar (what
///      `taiki-e/upload-rust-binary-action` emits with `checksum: sha256`).
///
/// A **404 on the aggregate falls through** to the sidecar (the two conventions
/// are mutually exclusive per release). A malformed hash in EITHER →
/// [`InstallError::ChecksumMalformed`]; an aggregate that exists but lacks the
/// asset's line → [`InstallError::ChecksumAbsent`] (a genuine gap in praxec's
/// own convention); BOTH 404 → [`InstallError::ChecksumUnavailable`]. Every arm
/// is fail-CLOSED — resolution never yields an unverified "ok".
fn resolve_expected_sha256(
    io: &dyn InstallerIo,
    host: &Host,
    asset: &str,
    aggregate_url: &str,
    sidecar_url: &str,
) -> Result<String, InstallError> {
    let download_err = |url: &str, e: InstallError| InstallError::Download {
        url: url.to_string(),
        os: host.os.clone(),
        arch: host.arch.clone(),
        reason: e.to_string(),
    };

    // (1) aggregate — a 404 falls through to the sidecar; any other transport
    // failure is fatal (we cannot know whether integrity is establishable).
    match io.http_get(aggregate_url) {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            match expected_sha256(&text, asset) {
                ChecksumLookup::Found(hash) => return Ok(hash.to_string()),
                ChecksumLookup::Malformed(token) => {
                    return Err(InstallError::ChecksumMalformed {
                        url: aggregate_url.to_string(),
                        asset: asset.to_string(),
                        token: token.to_string(),
                    });
                }
                ChecksumLookup::Absent => {
                    return Err(InstallError::ChecksumAbsent {
                        url: aggregate_url.to_string(),
                        asset: asset.to_string(),
                    });
                }
            }
        }
        Err(InstallError::NotFound { .. }) => { /* fall through to the sidecar */ }
        Err(e) => return Err(download_err(aggregate_url, e)),
    }

    // (2) per-asset sidecar — its absence (both 404) is the fail-CLOSED
    // ChecksumUnavailable naming both URLs; a present-but-garbage body is
    // ChecksumMalformed.
    match io.http_get(sidecar_url) {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            match sidecar_sha256(&text) {
                ChecksumLookup::Found(hash) => Ok(hash.to_string()),
                ChecksumLookup::Malformed(token) => Err(InstallError::ChecksumMalformed {
                    url: sidecar_url.to_string(),
                    asset: asset.to_string(),
                    token: token.to_string(),
                }),
                // sidecar_sha256 never yields Absent — an empty body is Malformed.
                ChecksumLookup::Absent => Err(InstallError::ChecksumMalformed {
                    url: sidecar_url.to_string(),
                    asset: asset.to_string(),
                    token: String::new(),
                }),
            }
        }
        Err(InstallError::NotFound { .. }) => Err(InstallError::ChecksumUnavailable {
            aggregate_url: aggregate_url.to_string(),
            sidecar_url: sidecar_url.to_string(),
            asset: asset.to_string(),
        }),
        Err(e) => Err(download_err(sidecar_url, e)),
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
    let aggregate_url = format!("{page}/download/v{version}/checksums.sha256");
    let sidecar_url = format!("{page}/download/v{version}/{asset}.sha256");

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

    // Integrity: resolve the expected hash from EITHER checksum convention
    // (aggregate, then per-asset sidecar). No hash available → error; malformed
    // token → error; mismatch → refuse (outcome). In NO case is a byte written
    // before the hash verifies — every arm is fail-CLOSED.
    let expected = resolve_expected_sha256(io, host, &asset, &aggregate_url, &sidecar_url)?;
    let actual = sha256_hex(&asset_bytes);
    if !actual.eq_ignore_ascii_case(&expected) {
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
    // Record the install-time version marker so a managed release binary reports
    // a truthful version even though the MCP tool binaries have no `--version`
    // (they ignore the flag and start their stdio server). `installed_version`
    // reads this back — closing the loop for both idempotency and currency.
    io.write_version_marker(&bin_dir, command, version)?;
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
        /// url → bytes (a missing url is a 404-equivalent → `NotFound`, so the
        /// checksum resolver falls through aggregate → sidecar as in production).
        responses: HashMap<String, Vec<u8>>,
        /// what `installed_version` reports when no marker was recorded.
        installed: Option<String>,
        bin_dir: PathBuf,
        /// recorded executable placements: (dir, name, byte-len).
        writes: RefCell<Vec<(PathBuf, String, usize)>>,
        /// recorded install-time version markers: (dir, command) → version.
        markers: RefCell<HashMap<(PathBuf, String), String>>,
    }

    impl InstallerIo for FakeIo {
        fn http_get(&self, url: &str) -> Result<Vec<u8>, InstallError> {
            self.responses
                .get(url)
                .cloned()
                .ok_or_else(|| InstallError::NotFound {
                    url: url.to_string(),
                })
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
        fn write_version_marker(
            &self,
            dir: &Path,
            name: &str,
            version: &str,
        ) -> Result<(), InstallError> {
            self.markers
                .borrow_mut()
                .insert((dir.to_path_buf(), name.to_string()), version.to_string());
            Ok(())
        }
        fn installed_version(&self, dir: &Path, name: &str) -> Option<String> {
            // Marker-first, mirroring production: a recorded marker wins over the
            // `installed` fallback, so install idempotency agrees with the marker.
            self.markers
                .borrow()
                .get(&(dir.to_path_buf(), name.to_string()))
                .cloned()
                .or_else(|| self.installed.clone())
        }
        fn bin_dir(&self) -> Result<PathBuf, InstallError> {
            Ok(self.bin_dir.clone())
        }
        fn which(&self, _cmd: &str) -> bool {
            false
        }
        fn docker_pull(&self, _image_ref: &str) -> Result<(), InstallError> {
            Ok(())
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
    fn managed_bin_dir_is_config_dir_praxec_bin() {
        // The one managed-bin-dir definition is `<config-dir>/praxec/bin`. Assert
        // the shape only when the host exposes a config dir (CI always does); a
        // `None` host is the fail-safe path, covered by the delegation test below.
        if let Some(cfg) = dirs::config_dir() {
            assert_eq!(managed_bin_dir(), Some(cfg.join("praxec").join("bin")));
        }
    }

    #[test]
    fn managed_binary_in_resolves_an_existing_file_and_none_otherwise() {
        // The ONE managed-bin existence predicate: absent → None; a placed file
        // (bare name, or `.exe` on Windows) → Some(that path).
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            managed_binary_in(dir.path(), "widget"),
            None,
            "absent binary is None"
        );
        let file_name = if cfg!(windows) {
            "widget.exe"
        } else {
            "widget"
        };
        std::fs::write(dir.path().join(file_name), b"x").unwrap();
        assert_eq!(
            managed_binary_in(dir.path(), "widget"),
            Some(dir.path().join(file_name)),
            "a placed binary resolves to its path (`.exe`-aware)"
        );
    }

    #[test]
    fn real_installer_bin_dir_delegates_to_the_one_managed_dir() {
        // Single source of truth: the trait impl resolves the exact same path as
        // the free fn (both `Some` on a normal host, or both absent).
        assert_eq!(RealInstallerIo.bin_dir().ok(), managed_bin_dir());
    }

    #[test]
    fn command_resolves_true_for_an_existing_explicit_path_false_for_a_missing_one() {
        let dir = tempfile::tempdir().unwrap();
        let present = dir.path().join("mytool");
        std::fs::write(&present, b"x").unwrap();
        // An explicit path to a file that exists resolves...
        assert!(command_resolves(present.to_str().unwrap()));
        // ...and a machine-specific absolute path that does NOT exist (e.g. a
        // Linux `~/.cargo/bin/...` command shipped to a Windows host) does not —
        // which is exactly what makes an optional connection skip.
        let missing = dir.path().join("sub").join("ghost");
        assert!(!command_resolves(missing.to_str().unwrap()));
    }

    #[test]
    fn command_resolves_false_for_a_bare_name_absent_from_path_and_spawn_dirs() {
        assert!(!command_resolves("praxec-no-such-binary-xyzzy-9f3c1"));
    }

    #[test]
    fn spawn_path_dirs_leads_with_the_managed_bin_dir_when_present() {
        // The managed bin dir (if the host has a config dir) is the FIRST spawn
        // dir, ahead of the conventional user tool dirs — install precedence.
        let dirs = spawn_path_dirs();
        if let Some(managed) = managed_bin_dir() {
            assert_eq!(dirs.first(), Some(&managed));
        }
    }

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
        // A well-formed sha256 is 64 hex chars; use real-shape hashes so the M3
        // shape-validation passes and the match/basename logic is what's tested.
        let good = "a".repeat(64);
        let other = "b".repeat(64);
        let file = format!(
            "{good}  *./cpm-planner-x86_64-unknown-linux-gnu.tar.gz\n{other}  other-asset.zip\n"
        );
        assert_eq!(
            expected_sha256(&file, "cpm-planner-x86_64-unknown-linux-gnu.tar.gz"),
            ChecksumLookup::Found(good.as_str())
        );
    }

    #[test]
    fn expected_sha256_reports_absent_when_no_line_matches() {
        let good = "c".repeat(64);
        let file = format!("{good}  some-other-asset.tar.gz\n");
        assert_eq!(
            expected_sha256(&file, "cpm-planner-x86_64-unknown-linux-gnu.tar.gz"),
            ChecksumLookup::Absent
        );
    }

    #[test]
    fn expected_sha256_reports_malformed_for_a_short_hash_token() {
        // A matched line whose hash token is not 64 hex chars is malformed, NOT
        // absent — M3 fail-CLOSED: never verify against a truncated checksum.
        let file = "deadbeef0  cpm-planner-x86_64-unknown-linux-gnu.tar.gz\n";
        assert_eq!(
            expected_sha256(file, "cpm-planner-x86_64-unknown-linux-gnu.tar.gz"),
            ChecksumLookup::Malformed("deadbeef0")
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
    fn malformed_checksum_token_fails_closed_and_writes_nothing() {
        // M3: the asset's checksum line carries a 10-char garbage hash (not 64
        // hex) → CHECKSUM_MALFORMED, fail-CLOSED, nothing placed.
        let page = "https://github.com/praxec/cpm-planner/releases";
        let host = Host {
            os: "linux".into(),
            arch: "x86_64".into(),
        };
        let asset = "cpm-planner-x86_64-unknown-linux-gnu.tar.gz";
        let asset_bytes = make_targz("cpm-planner", b"real bytes");
        let mut responses = HashMap::new();
        responses.insert(format!("{page}/download/v0.0.2/{asset}"), asset_bytes);
        responses.insert(
            format!("{page}/download/v0.0.2/checksums.sha256"),
            format!("deadbeef00  {asset}\n").into_bytes(),
        );
        let io = FakeIo {
            responses,
            bin_dir: "/fake/bin".into(),
            ..Default::default()
        };

        let err = install_release(&tool("cpm-planner", "0.0.2", page), &host, &io).unwrap_err();
        assert!(
            matches!(err, InstallError::ChecksumMalformed { .. }),
            "got {err:?}"
        );
        assert!(err.to_string().contains("CHECKSUM_MALFORMED"));
        assert_eq!(
            io.writes.borrow().len(),
            0,
            "no binary placed on a malformed checksum"
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

    // ── PART A: per-asset sidecar checksum convention ────────────────────────

    #[test]
    fn sidecar_sha256_parses_a_bare_hex_body() {
        let hex = "a".repeat(64);
        let body = format!("{hex}\n");
        assert_eq!(sidecar_sha256(&body), ChecksumLookup::Found(hex.as_str()));
    }

    #[test]
    fn sidecar_sha256_parses_the_hex_two_space_asset_body() {
        // sha256sum's `<hex>  <asset>` shape — the hash is the first token.
        let hex = "b".repeat(64);
        let body = format!("{hex}  cpm-planner-x86_64-unknown-linux-gnu.tar.gz\n");
        assert_eq!(sidecar_sha256(&body), ChecksumLookup::Found(hex.as_str()));
    }

    #[test]
    fn sidecar_sha256_reports_malformed_for_a_short_token_and_empty_body() {
        assert_eq!(
            sidecar_sha256("deadbeef00\n"),
            ChecksumLookup::Malformed("deadbeef00")
        );
        assert_eq!(sidecar_sha256("   \n"), ChecksumLookup::Malformed(""));
    }

    #[test]
    fn aggregate_404_falls_through_to_a_matching_sidecar_and_installs() {
        // THE BUG FIX: no aggregate `checksums.sha256` (a taiki-e release), but a
        // per-asset `<asset>.sha256` sidecar is present + matches → installs.
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
        // NO aggregate registered (→ 404). Only the per-asset sidecar exists.
        responses.insert(
            format!("{page}/download/v0.0.2/{asset}.sha256"),
            format!("{}  {asset}\n", sha256_hex(&asset_bytes)).into_bytes(),
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
            },
            "aggregate 404 must fall through to the sidecar, not abort"
        );
        assert_eq!(io.writes.borrow().len(), 1, "the binary was placed");
    }

    #[test]
    fn both_checksum_conventions_absent_is_unavailable_and_writes_nothing() {
        // Neither aggregate NOR sidecar exists (both 404) → fail-CLOSED with
        // ChecksumUnavailable naming BOTH URLs; no binary placed.
        let page = "https://github.com/praxec/cpm-planner/releases";
        let host = Host {
            os: "linux".into(),
            arch: "x86_64".into(),
        };
        let asset = "cpm-planner-x86_64-unknown-linux-gnu.tar.gz";
        let asset_bytes = make_targz("cpm-planner", b"real bytes");
        let mut responses = HashMap::new();
        responses.insert(format!("{page}/download/v0.0.2/{asset}"), asset_bytes);
        // No checksum of either convention registered.
        let io = FakeIo {
            responses,
            bin_dir: "/fake/bin".into(),
            ..Default::default()
        };

        let err = install_release(&tool("cpm-planner", "0.0.2", page), &host, &io).unwrap_err();
        let (aggregate_url, sidecar_url, asset_named) = match &err {
            InstallError::ChecksumUnavailable {
                aggregate_url,
                sidecar_url,
                asset,
            } => (aggregate_url.clone(), sidecar_url.clone(), asset.clone()),
            other => panic!("expected ChecksumUnavailable, got {other:?}"),
        };
        assert!(
            aggregate_url.ends_with("/checksums.sha256"),
            "{aggregate_url}"
        );
        assert!(
            sidecar_url.ends_with(&format!("/{asset}.sha256")),
            "{sidecar_url}"
        );
        assert_eq!(asset_named, asset);
        assert!(err.to_string().contains("CHECKSUM_UNAVAILABLE"));
        assert_eq!(
            io.writes.borrow().len(),
            0,
            "an unverifiable asset is NEVER placed"
        );
    }

    #[test]
    fn sidecar_mismatch_refuses_and_writes_nothing() {
        // Aggregate 404 → sidecar present but for DIFFERENT bytes → Refused.
        let page = "https://github.com/praxec/cpm-planner/releases";
        let host = Host {
            os: "linux".into(),
            arch: "x86_64".into(),
        };
        let asset = "cpm-planner-x86_64-unknown-linux-gnu.tar.gz";
        let asset_bytes = make_targz("cpm-planner", b"real bytes");
        let mut responses = HashMap::new();
        responses.insert(format!("{page}/download/v0.0.2/{asset}"), asset_bytes);
        responses.insert(
            format!("{page}/download/v0.0.2/{asset}.sha256"),
            format!("{}\n", sha256_hex(b"tampered")).into_bytes(),
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
            "no binary placed on a sidecar checksum mismatch"
        );
    }

    // ── PART B: install-time version marker ──────────────────────────────────

    #[test]
    fn install_records_the_version_marker() {
        // After a successful install the version marker is recorded for the
        // command, holding the placed version — this is what currency reads back.
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
        assert!(matches!(out, InstallOutcome::Installed { .. }));
        assert_eq!(
            io.markers
                .borrow()
                .get(&(bin.clone(), "cpm-planner".to_string()))
                .map(String::as_str),
            Some("0.0.2"),
            "the install-time version marker records the placed version"
        );
        // Idempotency agrees with the marker: a second install short-circuits.
        let again = install_release(&tool("cpm-planner", "0.0.2", page), &host, &io).unwrap();
        assert_eq!(again, InstallOutcome::AlreadyCurrent);
    }

    #[test]
    fn version_marker_path_is_dot_command_dot_version() {
        // Namespaced so it never collides with the spawnable `<command>` binary.
        assert_eq!(
            version_marker_path(Path::new("/bin"), "cpm-planner"),
            PathBuf::from("/bin/.cpm-planner.version")
        );
    }
}
