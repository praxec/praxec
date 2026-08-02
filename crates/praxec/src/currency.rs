//! `doctor` tool-currency: is each `kind: mcp` connection actually up to date
//! with its source?
//!
//! [`crate::provision::detect`] answers *existence* (is the binary on PATH).
//! This answers *currency* (is what's installed the latest), which existence
//! cannot: a binary on PATH carries no link back to a source commit. The check
//! is **transport-aware**, because "up to date" means different things per
//! deployment:
//!
//!   - **local cargo binary** (`command: cpm-planner`): cargo records the
//!     install source in `~/.cargo/.crates2.json`. For a local-`path` install
//!     we compare the binary's mtime against that repo's HEAD commit — a
//!     commit newer than the build means "rebuild" (`TOOL_BEHIND_SOURCE`).
//!     Zero config: cargo already knows the source.
//!   - **docker** (`command: docker`, opt-in `source: { docker: img:tag }`):
//!     compare the local image digest against the registry's digest for that
//!     tag (`DOCKER_IMAGE_BEHIND`). Explicit `source` rather than parsing
//!     `docker run` args, which is not reliably decodable.
//!   - **remote** (`url:` — a `StreamableHttp` MCP): currency is the REMOTE
//!     operator's responsibility, not the local host's. The honest check is
//!     "reachable + here is the version it advertises on `initialize`"
//!     (`REMOTE_MCP_ADVISORY`); only when the connection declares
//!     `source: { expect_version }` do we compare and warn on drift
//!     (`REMOTE_MCP_VERSION_MISMATCH`).
//!   - **npx** (`command: npx …@latest`): tracks upstream every launch — always
//!     current, reported as info; a pinned `@x.y.z` is noted.
//!
//! Everything here is ADVISORY (warn/info) — a stale-but-working tool must not
//! block, exactly as a missing tool is only a warning. All I/O (cargo metadata,
//! git, docker, the remote handshake) is behind the [`CurrencyIo`] seam so the
//! decision logic is unit-tested without touching the host.

use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Warn,
    Info,
}

/// One `kind: mcp` connection projected from the resolved config.
#[derive(Debug, Clone, Default)]
pub struct ConnSpec {
    pub name: String,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub url: Option<String>,
    /// The optional `source:` block (typed override — `{ docker: … }` /
    /// `{ expect_version: … }`).
    pub source: Option<Value>,
}

/// How a connection is sourced — selects the currency probe. Classification is
/// pure; the actual probes live behind [`CurrencyIo`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnSource {
    /// A binary `praxec tools install` placed in the managed bin dir
    /// (`<config-dir>/praxec/bin`). This SUPERSEDES a stale cargo copy for
    /// currency: the managed release binary is what praxec spawns (its dir is
    /// prepended to the child PATH), so we compare its `--version` against the
    /// registry's declared `version` for that command.
    ManagedRelease {
        command: String,
        /// The version the managed binary reports (`--version`), if readable.
        installed_version: Option<String>,
        /// The version the registry declares for this command, if any.
        expected_version: Option<String>,
    },
    /// cargo-installed binary whose install source is a local git `path`.
    LocalCargoPath {
        command: String,
        version: String,
        repo: PathBuf,
    },
    /// cargo-installed from crates.io / a git URL — installed version is known,
    /// but "latest" is not locally derivable without a network fetch.
    LocalCargoOther {
        command: String,
        version: String,
        source: String,
    },
    /// `command: docker` with an explicit `source: { docker: "img:tag" }`.
    Docker { image: String },
    /// remote MCP over HTTP (`url:`), with an optional declared expected version.
    Remote {
        url: String,
        expect_version: Option<String>,
    },
    /// npx-launched package (`@latest` tracks upstream; a pin is captured).
    Npx { pkg: String, pinned: Option<String> },
    /// `command: docker` but no `source: { docker }` — we will not guess the
    /// image out of `docker run` args, so currency is opt-in here.
    DockerUndeclared,
    /// on PATH but not cargo-installed and no other derivable source.
    ExternalBinary { command: String },
    /// not on PATH at all — existence is [`crate::provision`]'s concern; skip.
    NotResolvable { command: String },
}

/// One currency finding for a connection. Advisory only.
#[derive(Debug, Clone)]
pub struct CurrencyDiagnostic {
    pub connection: String,
    /// Stable code: `TOOL_BEHIND_SOURCE`, `TOOL_CURRENT`, `DOCKER_IMAGE_BEHIND`,
    /// `DOCKER_IMAGE_CURRENT`, `DOCKER_SOURCE_UNDECLARED`, `REMOTE_MCP_ADVISORY`,
    /// `REMOTE_MCP_VERSION_MISMATCH`, `REMOTE_MCP_UNREACHABLE`, `NPX_TRACKS_LATEST`,
    /// `EXTERNAL_UNCHECKABLE`, `CURRENCY_UNKNOWN`.
    pub code: &'static str,
    pub severity: Severity,
    pub message: String,
}

/// The host-touching operations, injectable so the decision logic is pure in
/// tests. Every method is fallible/optional — a probe that can't run yields
/// `None`, degrading to an honest "unknown" rather than a false verdict.
pub trait CurrencyIo {
    /// Unix mtime (secs) of the resolved binary for `command`, if on PATH.
    fn binary_mtime(&self, command: &str) -> Option<i64>;
    /// Parsed `~/.cargo/.crates2.json`, if readable.
    fn crates2(&self) -> Option<Value>;
    /// Unix time (secs) of a local git repo's HEAD commit.
    fn git_head_time(&self, repo: &Path) -> Option<i64>;
    /// Local docker image digest (`RepoDigests`/`Id`) for an image ref.
    fn docker_local_digest(&self, image: &str) -> Option<String>;
    /// Registry digest for `image:tag` (`docker manifest inspect`).
    fn docker_registry_digest(&self, image: &str) -> Option<String>;
    /// Version a remote MCP advertises on `initialize` (bounded), if reachable.
    fn remote_version(&self, url: &str) -> Option<String>;

    /// Does the praxec-managed bin dir (`<config-dir>/praxec/bin`) contain a
    /// spawnable binary for `command`? Default `false` keeps managed-dir
    /// awareness opt-in per impl and the decision logic pure in tests.
    fn managed_binary_exists(&self, _command: &str) -> bool {
        false
    }

    /// The version the managed binary for `command` reports (`--version`), if
    /// readable — `None` when absent or unparseable (an honest "unknown",
    /// never a false verdict).
    fn managed_binary_version(&self, _command: &str) -> Option<String> {
        None
    }
}

/// Find the cargo install whose `bins` includes `command` (or whose crate name
/// IS `command`), returning `(installed_version, source_string)`. Pure over the
/// parsed `.crates2.json`. Keys look like
/// `"cpm-planner 0.0.2 (path+file:///home/mc/working/cpm-planner)"`.
pub fn crates2_lookup(crates2: &Value, command: &str) -> Option<(String, String)> {
    let installs = crates2.get("installs")?.as_object()?;
    for (key, val) in installs {
        let (namever, source) = key.rsplit_once(" (")?;
        let source = source.strip_suffix(')')?;
        let mut toks = namever.split(' ');
        let name = toks.next()?;
        let version = toks.next().unwrap_or("");
        let bin_match = val
            .get("bins")
            .and_then(Value::as_array)
            .map(|b| b.iter().any(|x| x.as_str() == Some(command)))
            .unwrap_or(false);
        if bin_match || name == command {
            return Some((version.to_string(), source.to_string()));
        }
    }
    None
}

/// Parse the npm package + optional pinned version off an `npx` invocation's
/// args. `["@playwright/mcp@latest", …]` → `("@playwright/mcp", Some("latest"))`;
/// `["-y", "corpus-mcp"]` → `("corpus-mcp", None)`. Skips npx flags (`-y`,
/// `--yes`, `--package`/`-p` take the NEXT arg as the package).
pub fn parse_npx_pkg(args: &[String]) -> Option<(String, Option<String>)> {
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "-p" || a == "--package" {
            i += 2; // the package is this flag's value — take it below via i-1
            if let Some(pkg) = args.get(i - 1) {
                return Some(split_pkg_version(pkg));
            }
            continue;
        }
        if a.starts_with('-') {
            i += 1;
            continue;
        }
        return Some(split_pkg_version(a));
    }
    None
}

/// Split `"@scope/name@version"` / `"name@version"` into (pkg, Some(version)),
/// preserving a leading scope `@`. No `@version` → (pkg, None).
fn split_pkg_version(spec: &str) -> (String, Option<String>) {
    let (scope, rest) = if let Some(stripped) = spec.strip_prefix('@') {
        ("@", stripped)
    } else {
        ("", spec)
    };
    match rest.split_once('@') {
        Some((name, ver)) => (format!("{scope}{name}"), Some(ver.to_string())),
        None => (format!("{scope}{rest}"), None),
    }
}

/// Read `expect_version` from a connection's `source:` block (`{ expect_version }`
/// or `{ remote: { expect_version } }`).
fn expect_version_of(source: &Option<Value>) -> Option<String> {
    let s = source.as_ref()?;
    s.get("expect_version")
        .or_else(|| s.pointer("/remote/expect_version"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Classify a connection into the currency probe it needs. Pure given the
/// cargo metadata, a PATH-existence predicate, and a managed-bin-dir probe (all
/// injected). `managed(command)` returns `Some((installed_version,
/// expected_version))` when a managed release binary exists for `command` — the
/// tuple carries the binary's reported version and the registry's declared one.
pub fn classify(
    spec: &ConnSpec,
    crates2: Option<&Value>,
    on_path: impl Fn(&str) -> bool,
    #[allow(clippy::type_complexity)] managed: impl Fn(&str) -> Option<(Option<String>, Option<String>)>,
) -> ConnSource {
    // A URL connection is a remote MCP regardless of any command.
    if let Some(url) = &spec.url {
        return ConnSource::Remote {
            url: url.clone(),
            expect_version: expect_version_of(&spec.source),
        };
    }
    let Some(command) = &spec.command else {
        return ConnSource::NotResolvable {
            command: String::new(),
        };
    };
    if command == "docker" {
        return match spec
            .source
            .as_ref()
            .and_then(|s| s.get("docker"))
            .and_then(Value::as_str)
        {
            Some(image) => ConnSource::Docker {
                image: image.to_string(),
            },
            None => ConnSource::DockerUndeclared,
        };
    }
    if command == "npx" {
        if let Some((pkg, pinned)) = parse_npx_pkg(&spec.args) {
            return ConnSource::Npx { pkg, pinned };
        }
    }
    // A praxec-installed release binary in the managed bin dir supersedes a
    // (possibly stale) cargo copy for currency — it is what praxec actually
    // spawns. Checked BEFORE the crates2/PATH arms so managed wins.
    if let Some((installed_version, expected_version)) = managed(command) {
        return ConnSource::ManagedRelease {
            command: command.clone(),
            installed_version,
            expected_version,
        };
    }
    // A cargo-installed binary: cargo already recorded its source.
    if let Some((version, source)) = crates2.and_then(|c| crates2_lookup(c, command)) {
        if let Some(path) = source.strip_prefix("path+file://") {
            return ConnSource::LocalCargoPath {
                command: command.clone(),
                version,
                repo: PathBuf::from(path),
            };
        }
        return ConnSource::LocalCargoOther {
            command: command.clone(),
            version,
            source,
        };
    }
    if on_path(command) {
        return ConnSource::ExternalBinary {
            command: command.clone(),
        };
    }
    ConnSource::NotResolvable {
        command: command.clone(),
    }
}

/// The pure local-cargo verdict: given the binary's build mtime and the source
/// repo's HEAD commit time, is the binary behind? A commit strictly newer than
/// the build means the source moved since — rebuild.
pub fn local_cargo_behind(bin_mtime: i64, head_commit_time: i64) -> bool {
    head_commit_time > bin_mtime
}

/// Produce the currency diagnostics for all connections. The orchestrator is
/// pure over [`CurrencyIo`]; swap a fake in tests. `registry_versions` maps a
/// command to the version the registry declares for it (empty when no registry
/// is loaded) — the `expected` side of a [`ConnSource::ManagedRelease`] verdict.
pub fn check_currency(
    specs: &[ConnSpec],
    registry_versions: &HashMap<String, String>,
    io: &dyn CurrencyIo,
) -> Vec<CurrencyDiagnostic> {
    let crates2 = io.crates2();
    let mut out = Vec::new();
    for spec in specs {
        // classification only needs cheap existence predicates + the managed
        // probe (existence → version, plus the registry's expected version).
        let source = classify(
            spec,
            crates2.as_ref(),
            |cmd| io.binary_mtime(cmd).is_some(),
            |cmd| {
                io.managed_binary_exists(cmd).then(|| {
                    (
                        io.managed_binary_version(cmd),
                        registry_versions.get(cmd).cloned(),
                    )
                })
            },
        );
        let diag = diagnose(&spec.name, &source, io);
        if let Some(d) = diag {
            out.push(d);
        }
    }
    out
}

fn diag(
    connection: &str,
    code: &'static str,
    severity: Severity,
    message: String,
) -> CurrencyDiagnostic {
    CurrencyDiagnostic {
        connection: connection.to_string(),
        code,
        severity,
        message,
    }
}

/// Turn one classified source into a diagnostic (or `None` when there is simply
/// nothing worth reporting — e.g. a command not on PATH is provision's concern).
fn diagnose(name: &str, source: &ConnSource, io: &dyn CurrencyIo) -> Option<CurrencyDiagnostic> {
    match source {
        ConnSource::ManagedRelease {
            command,
            installed_version,
            expected_version,
        } => match (installed_version, expected_version) {
            (Some(installed), Some(expected)) if installed == expected => Some(diag(
                name,
                "TOOL_CURRENT",
                Severity::Info,
                format!(
                    "`{command}` (v{installed}) is the praxec-managed release binary and matches \
                     the registry version."
                ),
            )),
            (Some(installed), Some(expected)) => Some(diag(
                name,
                "TOOL_BEHIND_REGISTRY",
                Severity::Warn,
                format!(
                    "`{command}` is STALE — the managed release binary is v{installed} but the \
                     registry declares v{expected}. Update it: `praxec tools install {command}` \
                     (or `praxec doctor --fix`)."
                ),
            )),
            _ => Some(diag(
                name,
                "CURRENCY_UNKNOWN",
                Severity::Info,
                format!(
                    "`{command}` is a praxec-managed release binary, but its version and/or the \
                     registry's expected version couldn't be read — currency unchecked."
                ),
            )),
        },
        ConnSource::LocalCargoPath {
            command,
            repo,
            version,
        } => {
            let bin_mtime = io.binary_mtime(command)?;
            let Some(head) = io.git_head_time(repo) else {
                return Some(diag(
                    name,
                    "CURRENCY_UNKNOWN",
                    Severity::Info,
                    format!(
                        "`{command}` (v{version}) installed from {}, but its git HEAD time \
                         couldn't be read — cannot judge currency.",
                        repo.display()
                    ),
                ));
            };
            if local_cargo_behind(bin_mtime, head) {
                Some(diag(
                    name,
                    "TOOL_BEHIND_SOURCE",
                    Severity::Warn,
                    format!(
                        "`{command}` is STALE — its source {} has commits newer than the installed \
                         binary. Update to the latest release binary: `praxec tools install \
                         {command}` (or `praxec doctor --fix`).",
                        repo.display(),
                    ),
                ))
            } else {
                Some(diag(
                    name,
                    "TOOL_CURRENT",
                    Severity::Info,
                    format!("`{command}` (v{version}) is current with its source."),
                ))
            }
        }
        ConnSource::LocalCargoOther {
            command,
            version,
            source,
        } => Some(diag(
            name,
            "CURRENCY_UNKNOWN",
            Severity::Info,
            format!(
                "`{command}` (v{version}) installed from {source} — currency needs a registry/remote \
                 fetch (not checked). Update to the latest release binary with `praxec tools install \
                 {command}` (or `praxec doctor --fix`) to be sure."
            ),
        )),
        ConnSource::Docker { image } => {
            let local = io.docker_local_digest(image);
            let remote = io.docker_registry_digest(image);
            match (local, remote) {
                (Some(l), Some(r)) if l == r => Some(diag(
                    name,
                    "DOCKER_IMAGE_CURRENT",
                    Severity::Info,
                    format!("docker image `{image}` matches the registry digest."),
                )),
                (Some(_), Some(_)) => Some(diag(
                    name,
                    "DOCKER_IMAGE_BEHIND",
                    Severity::Warn,
                    format!(
                        "docker image `{image}` differs from the registry digest for its tag — \
                         `docker pull {image}` to update."
                    ),
                )),
                _ => Some(diag(
                    name,
                    "CURRENCY_UNKNOWN",
                    Severity::Info,
                    format!(
                        "docker image `{image}` — could not read a local and/or registry digest \
                         (docker CLI or network unavailable); currency unchecked."
                    ),
                )),
            }
        }
        ConnSource::DockerUndeclared => Some(diag(
            name,
            "DOCKER_SOURCE_UNDECLARED",
            Severity::Info,
            "docker connection — declare `source: { docker: \"image:tag\" }` to enable a \
             registry-digest currency check (the run args are not decoded)."
                .to_string(),
        )),
        ConnSource::Remote {
            url,
            expect_version,
        } => {
            let advertised = io.remote_version(url);
            match (advertised, expect_version) {
                (Some(v), Some(want)) if &v != want => Some(diag(
                    name,
                    "REMOTE_MCP_VERSION_MISMATCH",
                    Severity::Warn,
                    format!(
                        "remote MCP at {url} advertises version {v}, but this connection expects \
                         {want}."
                    ),
                )),
                (Some(v), _) => Some(diag(
                    name,
                    "REMOTE_MCP_ADVISORY",
                    Severity::Info,
                    format!(
                        "remote MCP at {url} — reachable, advertises version {v}. Currency is the \
                         remote operator's responsibility, not this host's."
                    ),
                )),
                (None, _) => Some(diag(
                    name,
                    "REMOTE_MCP_UNREACHABLE",
                    Severity::Warn,
                    format!(
                        "remote MCP at {url} did not answer an initialize probe — unreachable or \
                         not speaking MCP."
                    ),
                )),
            }
        }
        ConnSource::Npx { pkg, pinned } => match pinned.as_deref() {
            None | Some("latest") => Some(diag(
                name,
                "NPX_TRACKS_LATEST",
                Severity::Info,
                format!("`npx {pkg}` fetches the latest published version at every launch."),
            )),
            Some(v) => Some(diag(
                name,
                "NPX_PINNED",
                Severity::Info,
                format!("`npx {pkg}@{v}` is pinned — it will not pick up newer releases."),
            )),
        },
        ConnSource::ExternalBinary { command } => Some(diag(
            name,
            "EXTERNAL_UNCHECKABLE",
            Severity::Info,
            format!(
                "`{command}` is on PATH but not cargo-installed from a known source — currency \
                 can't be determined."
            ),
        )),
        ConnSource::NotResolvable { .. } => None,
    }
}

/// Render the currency section for `doctor`. Empty input prints nothing.
pub fn format_currency(diags: &[CurrencyDiagnostic]) -> String {
    if diags.is_empty() {
        return String::new();
    }
    let mut out = String::from("tool currency (kind: mcp — installed vs latest source):\n");
    for d in diags {
        let tag = match d.severity {
            Severity::Warn => "warn",
            Severity::Info => "info",
        };
        out.push_str(&format!(
            "  {tag}  {}  {} — {}\n",
            d.code, d.connection, d.message
        ));
    }
    out
}

/// Project the resolved config's `connections:` into [`ConnSpec`]s.
pub fn conn_specs_from(config: &Value) -> Vec<ConnSpec> {
    let Some(conns) = config.pointer("/connections").and_then(Value::as_object) else {
        return Vec::new();
    };
    conns
        .iter()
        .filter(|(_, c)| c.get("kind").and_then(Value::as_str) == Some("mcp"))
        .map(|(name, c)| ConnSpec {
            name: name.clone(),
            command: c.get("command").and_then(Value::as_str).map(str::to_string),
            args: c
                .get("args")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            url: c.get("url").and_then(Value::as_str).map(str::to_string),
            source: c.get("source").cloned(),
        })
        .collect()
}

// ── production I/O: the real host probes ────────────────────────────────────

/// The production [`CurrencyIo`]: cargo metadata, git, docker, and a bounded
/// remote MCP `initialize` handshake. Every probe is best-effort — any failure
/// (tool absent, network down, unreadable file) yields `None`, which the
/// decision logic renders as an honest "unknown" rather than a false verdict.
pub struct RealCurrencyIo;

impl RealCurrencyIo {
    fn cargo_home() -> Option<PathBuf> {
        if let Some(h) = std::env::var_os("CARGO_HOME") {
            return Some(PathBuf::from(h));
        }
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo"))
    }
}

impl CurrencyIo for RealCurrencyIo {
    fn binary_mtime(&self, command: &str) -> Option<i64> {
        let path = which::which(command).ok()?;
        let mtime = std::fs::metadata(&path).ok()?.modified().ok()?;
        Some(mtime.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64)
    }

    fn crates2(&self) -> Option<Value> {
        let path = Self::cargo_home()?.join(".crates2.json");
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
    }

    fn git_head_time(&self, repo: &Path) -> Option<i64> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["log", "-1", "--format=%ct"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse::<i64>()
            .ok()
    }

    fn docker_local_digest(&self, image: &str) -> Option<String> {
        let out = std::process::Command::new("docker")
            .args([
                "image",
                "inspect",
                "--format",
                "{{index .RepoDigests 0}}",
                image,
            ])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        // RepoDigests entries are `image@sha256:…` — the manifest digest.
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .rsplit_once('@')
            .map(|(_, d)| d.to_string())
            .filter(|d| d.starts_with("sha256:"))
    }

    fn docker_registry_digest(&self, image: &str) -> Option<String> {
        // buildx imagetools yields the MANIFEST digest cleanly — the same kind
        // RepoDigests carries, so the two are comparable. If buildx is absent we
        // return None (honest unknown) rather than compare mismatched digest
        // kinds and raise a false "behind".
        let out = std::process::Command::new("docker")
            .args([
                "buildx",
                "imagetools",
                "inspect",
                image,
                "--format",
                "{{.Manifest.Digest}}",
            ])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        s.starts_with("sha256:").then_some(s)
    }

    fn managed_binary_exists(&self, command: &str) -> bool {
        let Some(dir) = praxec_core::provision_install::managed_bin_dir() else {
            return false;
        };
        if dir.join(command).is_file() {
            return true;
        }
        cfg!(windows) && dir.join(format!("{command}.exe")).is_file()
    }

    fn managed_binary_version(&self, command: &str) -> Option<String> {
        // Reuse the installer's version probe (path-exists → `--version` →
        // parse_version) against the ONE managed bin dir, so the version-parsing
        // rules never drift between install-idempotency and currency.
        use praxec_core::provision_install::InstallerIo;
        let dir = praxec_core::provision_install::managed_bin_dir()?;
        praxec_core::provision_install::RealInstallerIo.installed_version(&dir, command)
    }

    fn remote_version(&self, url: &str) -> Option<String> {
        let url = url.to_string();
        // A dedicated thread with its OWN current-thread runtime: safe whether or
        // not `doctor` runs inside a tokio runtime (a nested `block_on` panics).
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .ok()?;
            rt.block_on(async {
                use rmcp::ServiceExt;
                use rmcp::transport::StreamableHttpClientTransport;
                let transport = StreamableHttpClientTransport::<reqwest::Client>::from_uri(url);
                // `()` is a no-op ClientHandler; a 3s bound on the whole handshake.
                let client =
                    tokio::time::timeout(std::time::Duration::from_secs(3), ().serve(transport))
                        .await
                        .ok()?
                        .ok()?;
                let version = client.peer_info().map(|i| i.server_info.version.clone());
                let _ = client.cancel().await;
                version
            })
        })
        .join()
        .ok()
        .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A fake host: everything is data, nothing touches the machine.
    #[derive(Default)]
    struct FakeIo {
        crates2: Option<Value>,
        mtimes: std::collections::HashMap<String, i64>,
        head_times: std::collections::HashMap<String, i64>,
        docker_local: std::collections::HashMap<String, String>,
        docker_registry: std::collections::HashMap<String, String>,
        remote: std::collections::HashMap<String, String>,
        /// Managed bin dir: presence of the key = the binary exists; the value
        /// is the version its `--version` reports (`None` = unreadable).
        managed: std::collections::HashMap<String, Option<String>>,
    }
    impl CurrencyIo for FakeIo {
        fn binary_mtime(&self, c: &str) -> Option<i64> {
            self.mtimes.get(c).copied()
        }
        fn crates2(&self) -> Option<Value> {
            self.crates2.clone()
        }
        fn git_head_time(&self, repo: &Path) -> Option<i64> {
            self.head_times.get(repo.to_str().unwrap()).copied()
        }
        fn docker_local_digest(&self, image: &str) -> Option<String> {
            self.docker_local.get(image).cloned()
        }
        fn docker_registry_digest(&self, image: &str) -> Option<String> {
            self.docker_registry.get(image).cloned()
        }
        fn remote_version(&self, url: &str) -> Option<String> {
            self.remote.get(url).cloned()
        }
        fn managed_binary_exists(&self, command: &str) -> bool {
            self.managed.contains_key(command)
        }
        fn managed_binary_version(&self, command: &str) -> Option<String> {
            self.managed.get(command).cloned().flatten()
        }
    }

    /// No managed binaries — the closure classify wants when a test exercises
    /// only the cargo/docker/remote/npx arms.
    fn no_managed(_: &str) -> Option<(Option<String>, Option<String>)> {
        None
    }

    /// An empty registry-version map — the common `check_currency` argument when
    /// a test isn't exercising the `ManagedRelease` (registry) path.
    fn no_registry() -> HashMap<String, String> {
        HashMap::new()
    }

    fn crates2_with(command: &str, version: &str, source: &str) -> Value {
        json!({ "installs": { format!("{command} {version} ({source})"): { "bins": [command] } } })
    }

    #[test]
    fn crates2_lookup_finds_by_bin_and_parses_source() {
        let c = crates2_with(
            "cpm-planner",
            "0.0.2",
            "path+file:///home/mc/working/cpm-planner",
        );
        let (ver, src) = crates2_lookup(&c, "cpm-planner").unwrap();
        assert_eq!(ver, "0.0.2");
        assert_eq!(src, "path+file:///home/mc/working/cpm-planner");
    }

    #[test]
    fn classify_url_is_remote_even_with_a_command() {
        let spec = ConnSpec {
            name: "r".into(),
            command: Some("whatever".into()),
            url: Some("https://mcp.example.com".into()),
            source: Some(json!({ "expect_version": "2.0.0" })),
            ..Default::default()
        };
        assert_eq!(
            classify(&spec, None, |_| true, no_managed),
            ConnSource::Remote {
                url: "https://mcp.example.com".into(),
                expect_version: Some("2.0.0".into())
            }
        );
    }

    #[test]
    fn classify_docker_needs_explicit_source() {
        let undeclared = ConnSpec {
            name: "d".into(),
            command: Some("docker".into()),
            args: vec!["run".into(), "-i".into(), "ghcr.io/x/y:1".into()],
            ..Default::default()
        };
        assert_eq!(
            classify(&undeclared, None, |_| true, no_managed),
            ConnSource::DockerUndeclared
        );
        let declared = ConnSpec {
            source: Some(json!({ "docker": "ghcr.io/x/y:1" })),
            ..undeclared
        };
        assert_eq!(
            classify(&declared, None, |_| true, no_managed),
            ConnSource::Docker {
                image: "ghcr.io/x/y:1".into()
            }
        );
    }

    #[test]
    fn classify_local_cargo_path_from_crates2() {
        let c = crates2_with("cpm-planner", "0.0.2", "path+file:///repo/cpm");
        let spec = ConnSpec {
            name: "cpm".into(),
            command: Some("cpm-planner".into()),
            ..Default::default()
        };
        assert_eq!(
            classify(&spec, Some(&c), |_| true, no_managed),
            ConnSource::LocalCargoPath {
                command: "cpm-planner".into(),
                version: "0.0.2".into(),
                repo: PathBuf::from("/repo/cpm"),
            }
        );
    }

    #[test]
    fn classify_managed_release_wins_over_a_cargo_entry() {
        // A managed release binary exists AND a cargo crates2 entry exists for
        // the same command — managed must win (it's what praxec spawns).
        let c = crates2_with("cpm-planner", "0.0.2", "path+file:///repo/cpm");
        let spec = ConnSpec {
            name: "cpm".into(),
            command: Some("cpm-planner".into()),
            ..Default::default()
        };
        let source = classify(
            &spec,
            Some(&c),
            |_| true,
            |cmd| {
                (cmd == "cpm-planner")
                    .then(|| (Some("0.0.5".to_string()), Some("0.0.5".to_string())))
            },
        );
        assert_eq!(
            source,
            ConnSource::ManagedRelease {
                command: "cpm-planner".into(),
                installed_version: Some("0.0.5".into()),
                expected_version: Some("0.0.5".into()),
            }
        );
    }

    #[test]
    fn managed_release_matching_registry_is_current() {
        let mut io = FakeIo::default();
        io.managed
            .insert("cpm-planner".into(), Some("0.0.5".into()));
        let mut versions = HashMap::new();
        versions.insert("cpm-planner".to_string(), "0.0.5".to_string());
        let specs = vec![ConnSpec {
            name: "cpm".into(),
            command: Some("cpm-planner".into()),
            ..Default::default()
        }];
        let diags = check_currency(&specs, &versions, &io);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "TOOL_CURRENT");
        assert_eq!(diags[0].severity, Severity::Info);
    }

    #[test]
    fn managed_release_older_than_registry_is_behind() {
        let mut io = FakeIo::default();
        io.managed
            .insert("cpm-planner".into(), Some("0.0.4".into()));
        let mut versions = HashMap::new();
        versions.insert("cpm-planner".to_string(), "0.0.5".to_string());
        let specs = vec![ConnSpec {
            name: "cpm".into(),
            command: Some("cpm-planner".into()),
            ..Default::default()
        }];
        let diags = check_currency(&specs, &versions, &io);
        assert_eq!(diags[0].code, "TOOL_BEHIND_REGISTRY");
        assert_eq!(diags[0].severity, Severity::Warn);
        assert!(
            diags[0]
                .message
                .contains("praxec tools install cpm-planner")
                || diags[0].message.contains("praxec doctor --fix"),
            "remediation names the install path: {}",
            diags[0].message
        );
    }

    #[test]
    fn managed_release_without_expected_version_is_unknown() {
        let mut io = FakeIo::default();
        io.managed
            .insert("cpm-planner".into(), Some("0.0.5".into()));
        // no registry entry for the command → expected version unknown
        let specs = vec![ConnSpec {
            name: "cpm".into(),
            command: Some("cpm-planner".into()),
            ..Default::default()
        }];
        let diags = check_currency(&specs, &no_registry(), &io);
        assert_eq!(diags[0].code, "CURRENCY_UNKNOWN");
        assert_eq!(diags[0].severity, Severity::Info);
    }

    #[test]
    fn parse_npx_handles_flags_scope_and_pin() {
        assert_eq!(
            parse_npx_pkg(&["@playwright/mcp@latest".into()]),
            Some(("@playwright/mcp".into(), Some("latest".into())))
        );
        assert_eq!(
            parse_npx_pkg(&["-y".into(), "corpus-mcp".into()]),
            Some(("corpus-mcp".into(), None))
        );
        assert_eq!(
            parse_npx_pkg(&["-p".into(), "playwright@1.2.3".into(), "node".into()]),
            Some(("playwright".into(), Some("1.2.3".into())))
        );
    }

    #[test]
    fn local_cargo_stale_when_source_commit_is_newer() {
        assert!(
            local_cargo_behind(100, 200),
            "commit newer than build → behind"
        );
        assert!(
            !local_cargo_behind(200, 100),
            "build newer than commit → current"
        );
        assert!(!local_cargo_behind(100, 100), "same → current");
    }

    #[test]
    fn stale_local_tool_produces_a_warn_with_the_fix() {
        let c = crates2_with("cpm-planner", "0.0.2", "path+file:///repo/cpm");
        let mut io = FakeIo {
            crates2: Some(c),
            ..Default::default()
        };
        io.mtimes.insert("cpm-planner".into(), 1000);
        io.head_times.insert("/repo/cpm".into(), 2000); // source moved after build
        let specs = vec![ConnSpec {
            name: "cpm".into(),
            command: Some("cpm-planner".into()),
            ..Default::default()
        }];
        let diags = check_currency(&specs, &no_registry(), &io);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "TOOL_BEHIND_SOURCE");
        assert_eq!(diags[0].severity, Severity::Warn);
        // No parallel install path: the remediation points at the ONE installer
        // surface (a release binary via `praxec tools install` / `doctor --fix`),
        // never a `cargo install --path --force` source rebuild.
        assert!(
            diags[0]
                .message
                .contains("praxec tools install cpm-planner")
                || diags[0].message.contains("praxec doctor --fix"),
            "remediation names the release-binary path: {}",
            diags[0].message
        );
        assert!(
            !diags[0].message.contains("cargo install --path"),
            "cargo source-build remediation must be gone: {}",
            diags[0].message
        );
    }

    #[test]
    fn current_local_tool_is_info_not_warn() {
        let c = crates2_with("corpus", "0.0.1", "path+file:///repo/corpus");
        let mut io = FakeIo {
            crates2: Some(c),
            ..Default::default()
        };
        io.mtimes.insert("corpus".into(), 5000);
        io.head_times.insert("/repo/corpus".into(), 4000); // built after last commit
        let specs = vec![ConnSpec {
            name: "corpus".into(),
            command: Some("corpus".into()),
            ..Default::default()
        }];
        let diags = check_currency(&specs, &no_registry(), &io);
        assert_eq!(diags[0].code, "TOOL_CURRENT");
        assert_eq!(diags[0].severity, Severity::Info);
    }

    #[test]
    fn docker_digest_mismatch_warns() {
        let mut io = FakeIo::default();
        io.docker_local
            .insert("ghcr.io/x/y:1".into(), "sha256:aaa".into());
        io.docker_registry
            .insert("ghcr.io/x/y:1".into(), "sha256:bbb".into());
        let specs = vec![ConnSpec {
            name: "d".into(),
            command: Some("docker".into()),
            source: Some(json!({ "docker": "ghcr.io/x/y:1" })),
            ..Default::default()
        }];
        let diags = check_currency(&specs, &no_registry(), &io);
        assert_eq!(diags[0].code, "DOCKER_IMAGE_BEHIND");
        assert_eq!(diags[0].severity, Severity::Warn);
    }

    #[test]
    fn docker_digest_match_is_current() {
        let mut io = FakeIo::default();
        io.docker_local.insert("img:2".into(), "sha256:same".into());
        io.docker_registry
            .insert("img:2".into(), "sha256:same".into());
        let specs = vec![ConnSpec {
            name: "d".into(),
            command: Some("docker".into()),
            source: Some(json!({ "docker": "img:2" })),
            ..Default::default()
        }];
        assert_eq!(
            check_currency(&specs, &no_registry(), &io)[0].code,
            "DOCKER_IMAGE_CURRENT"
        );
    }

    #[test]
    fn remote_advertises_version_as_advisory() {
        let mut io = FakeIo::default();
        io.remote.insert("https://mcp.x".into(), "1.62.0".into());
        let specs = vec![ConnSpec {
            name: "r".into(),
            url: Some("https://mcp.x".into()),
            ..Default::default()
        }];
        let d = &check_currency(&specs, &no_registry(), &io)[0];
        assert_eq!(d.code, "REMOTE_MCP_ADVISORY");
        assert_eq!(d.severity, Severity::Info);
        assert!(d.message.contains("1.62.0"));
    }

    #[test]
    fn remote_expect_mismatch_warns() {
        let mut io = FakeIo::default();
        io.remote.insert("https://mcp.x".into(), "1.0.0".into());
        let specs = vec![ConnSpec {
            name: "r".into(),
            url: Some("https://mcp.x".into()),
            source: Some(json!({ "expect_version": "2.0.0" })),
            ..Default::default()
        }];
        let d = &check_currency(&specs, &no_registry(), &io)[0];
        assert_eq!(d.code, "REMOTE_MCP_VERSION_MISMATCH");
        assert_eq!(d.severity, Severity::Warn);
    }

    #[test]
    fn remote_unreachable_warns() {
        let io = FakeIo::default(); // no remote version registered
        let specs = vec![ConnSpec {
            name: "r".into(),
            url: Some("https://down.x".into()),
            ..Default::default()
        }];
        assert_eq!(
            check_currency(&specs, &no_registry(), &io)[0].code,
            "REMOTE_MCP_UNREACHABLE"
        );
    }

    #[test]
    fn npx_latest_is_informational_current() {
        let io = FakeIo::default();
        let specs = vec![ConnSpec {
            name: "b".into(),
            command: Some("npx".into()),
            args: vec!["@playwright/mcp@latest".into(), "--headless".into()],
            ..Default::default()
        }];
        let d = &check_currency(&specs, &no_registry(), &io)[0];
        assert_eq!(d.code, "NPX_TRACKS_LATEST");
        assert_eq!(d.severity, Severity::Info);
    }

    #[test]
    fn advisory_currency_never_implies_a_hard_failure() {
        // Every diagnostic this module emits is Warn or Info — never a blocker.
        let mut io = FakeIo {
            crates2: Some(crates2_with("t", "0.0.1", "path+file:///r")),
            ..Default::default()
        };
        io.mtimes.insert("t".into(), 1);
        io.head_times.insert("/r".into(), 999);
        let specs = vec![ConnSpec {
            name: "t".into(),
            command: Some("t".into()),
            ..Default::default()
        }];
        for d in check_currency(&specs, &no_registry(), &io) {
            assert!(matches!(d.severity, Severity::Warn | Severity::Info));
        }
    }

    #[test]
    fn conn_specs_from_projects_command_args_url_source() {
        let cfg = json!({ "connections": {
            "a": { "kind": "mcp", "command": "cpm-planner" },
            "b": { "kind": "mcp", "command": "npx", "args": ["x@latest"] },
            "c": { "kind": "mcp", "url": "https://r", "source": { "expect_version": "9" } },
            "skip": { "kind": "rest", "command": "nope" }
        }});
        let specs = conn_specs_from(&cfg);
        assert_eq!(specs.len(), 3, "only kind: mcp, got {specs:?}");
        let c = specs.iter().find(|s| s.name == "c").unwrap();
        assert_eq!(c.url.as_deref(), Some("https://r"));
        assert_eq!(expect_version_of(&c.source).as_deref(), Some("9"));
    }
}
