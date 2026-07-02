//! ADR-0006 — the confined-execution seam. `SandboxProvider` is the OS-agnostic
//! boundary; backends (bubblewrap, OCI, microVM) enforce the same [`SandboxSpec`].
//!
//! Confinement is **per-script policy** ([`ConfinementProfile`]). The default is
//! **unconfined** so existing `kind: script` workflows are unchanged (ADR-0006
//! FMECA §1 — no blanket deny that breaks scripts needing net/secrets). A
//! gateway may opt into strict mode, where an unprofiled script **fails fast**
//! rather than running unconfined or being silently denied.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::Value;

/// Network egress policy inside the sandbox. Default-deny is the confined
/// posture (ADR-0006); `Allow` is an explicit host:port allowlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Egress {
    DenyAll,
    Allow(Vec<String>),
}

/// Resource bounds for a confined run. A provider that cannot enforce these
/// MUST fail preflight for the agentic tier (ADR-0006 FMECA §2 — no unlimited
/// "sandbox", or a fork-bomb inside it takes down the host).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceLimits {
    pub wall_clock_secs: u64,
    pub max_pids: u32,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            wall_clock_secs: 300,
            max_pids: 256,
        }
    }
}

/// Per-script confinement policy, parsed from a `kind: script` executor's
/// `confinement:` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfinementProfile {
    pub egress: Egress,
    /// Ambient env var names allowed to pass into the sandbox (everything else
    /// is scrubbed). Empty = a clean environment.
    pub env_allowlist: Vec<String>,
    pub limits: ResourceLimits,
}

impl ConfinementProfile {
    /// The default confined posture: deny egress, scrub the environment,
    /// default resource limits.
    pub fn confined() -> Self {
        Self {
            egress: Egress::DenyAll,
            env_allowlist: Vec::new(),
            limits: ResourceLimits::default(),
        }
    }

    /// Parse the executor config's `confinement` field. Fail-fast on an invalid
    /// shape — never a silent default (ADR-0006 / project no-fallback rule):
    /// - absent / `null` / `"none"` → `Ok(None)` (run unconfined),
    /// - `"confined"` → `Ok(Some(confined-default))`,
    /// - an object → `Ok(Some(..))` with explicit `egress` / `env` / `limits`,
    /// - anything else → `Err`.
    pub fn from_executor_config(cfg: &Value) -> anyhow::Result<Option<Self>> {
        match cfg.get("confinement") {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) if s == "none" => Ok(None),
            Some(Value::String(s)) if s == "confined" => Ok(Some(Self::confined())),
            Some(Value::String(other)) => anyhow::bail!(
                "INVALID_CONFINEMENT: `confinement` string must be \"none\" or \"confined\" \
                 (got \"{other}\"); use an object for a custom profile"
            ),
            Some(Value::Object(obj)) => {
                let egress = match obj.get("egress") {
                    None | Some(Value::Null) => Egress::DenyAll,
                    Some(Value::String(s)) if s == "deny" => Egress::DenyAll,
                    Some(Value::Array(a)) => Egress::Allow(
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect(),
                    ),
                    Some(other) => anyhow::bail!(
                        "INVALID_CONFINEMENT: `confinement.egress` must be \"deny\" or an array \
                         of host:port allowlist entries (got {})",
                        kind_of(other)
                    ),
                };
                let env_allowlist = match obj.get("env") {
                    None | Some(Value::Null) => Vec::new(),
                    Some(Value::Array(a)) => a
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect(),
                    Some(other) => anyhow::bail!(
                        "INVALID_CONFINEMENT: `confinement.env` must be an array of env var \
                         names to pass through (got {})",
                        kind_of(other)
                    ),
                };
                Ok(Some(Self {
                    egress,
                    env_allowlist,
                    limits: ResourceLimits::default(),
                }))
            }
            Some(other) => anyhow::bail!(
                "INVALID_CONFINEMENT: `confinement` must be \"none\", \"confined\", or an object \
                 (got {})",
                kind_of(other)
            ),
        }
    }
}

fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// What to run, confined. For Tier-2 production scripts the `workspace` is the
/// real (lock-coordinated) dir; Tier-1 disposable copies come later (ADR-0006
/// Coordination §). `command[0]` is the program; the rest are argv.
pub struct SandboxSpec {
    pub workspace: Option<PathBuf>,
    pub command: Vec<String>,
    /// Host files to make available read-only *at their own path* inside the
    /// sandbox (e.g. the script's temp body) — otherwise the fresh tmpfs hides
    /// them and the command can't find what it's meant to run.
    pub ro_binds: Vec<PathBuf>,
    /// Environment to set inside the sandbox (praxec-supplied + operator-
    /// declared). The ambient host env is scrubbed except `env_allowlist`.
    pub env: Vec<(String, String)>,
    pub egress: Egress,
    pub env_allowlist: Vec<String>,
    pub limits: ResourceLimits,
}

/// The captured result of a confined run.
#[derive(Debug)]
pub struct SandboxOutput {
    pub code: Option<i32>,
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Whether a backend is usable on THIS host right now — a **functional** check
/// (run a trivial confined op + assert isolation), never mere presence
/// (ADR-0006 Preflight §).
#[derive(Debug, Clone)]
pub struct Preflight {
    pub usable: bool,
    pub detail: String,
    pub install_hint: Option<String>,
}

/// The confinement contract. Every backend enforces the same `SandboxSpec`;
/// only the mechanism differs.
#[async_trait]
pub trait SandboxProvider: Send + Sync {
    fn preflight(&self) -> Preflight;
    async fn run(&self, spec: &SandboxSpec) -> anyhow::Result<SandboxOutput>;
}

/// Whether a usable systemd **user** scope is available for cgroup limits — a
/// one-time functional probe (presence ≠ functional: catches a present binary
/// with no running user manager), cached for the process lifetime.
fn cgroup_scope_available() -> bool {
    use std::sync::OnceLock;
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        std::process::Command::new("systemd-run")
            .args([
                "--user",
                "--scope",
                "--quiet",
                "--collect",
                "--",
                "/bin/true",
            ])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

/// ADR-0006 — the Linux-native fast-path backend (bubblewrap). Rootless,
/// daemonless, per-invocation. Validated by the ADR-0006 sandbox-exec spike.
#[derive(Default)]
pub struct BwrapProvider;

impl BwrapProvider {
    pub fn new() -> Self {
        Self
    }

    /// The read-only runtime scaffold shared by preflight + run: `/usr` plus the
    /// `/bin`,`/lib` symlinks Ubuntu-style layouts need.
    fn scaffold() -> Vec<String> {
        [
            "--ro-bind",
            "/usr",
            "/usr",
            "--symlink",
            "usr/bin",
            "/bin",
            "--symlink",
            "usr/lib",
            "/lib",
            "--symlink",
            "usr/lib64",
            "/lib64",
            "--symlink",
            "usr/sbin",
            "/sbin",
        ]
        .into_iter()
        .map(String::from)
        .collect()
    }
}

#[async_trait]
impl SandboxProvider for BwrapProvider {
    fn preflight(&self) -> Preflight {
        // Functional, not presence: bubblewrap must be able to *construct* a
        // user+net namespace and run. This is what catches "binary present but
        // unprivileged user namespaces disabled" — which a `which bwrap` misses.
        let mut args = vec!["--unshare-user".to_string(), "--unshare-net".to_string()];
        args.extend(Self::scaffold());
        args.push("/usr/bin/true".to_string());
        match std::process::Command::new("bwrap").args(&args).output() {
            Ok(o) if o.status.success() => {
                Preflight { usable: true, detail: "bubblewrap usable (namespaces ok)".into(), install_hint: None }
            }
            Ok(o) => Preflight {
                usable: false,
                detail: format!("bubblewrap present but unusable: {}", String::from_utf8_lossy(&o.stderr).trim()),
                install_hint: Some(
                    "enable unprivileged user namespaces (e.g. sysctl kernel.unprivileged_userns_clone=1) \
                     or use a container/VM backend"
                        .into(),
                ),
            },
            Err(e) => Preflight {
                usable: false,
                detail: format!("bubblewrap not runnable: {e}"),
                install_hint: Some("install bubblewrap (`apt install bubblewrap`) or use a container/VM backend".into()),
            },
        }
    }

    async fn run(&self, spec: &SandboxSpec) -> anyhow::Result<SandboxOutput> {
        // The bwrap backend confines egress all-or-nothing. A selective
        // allowlist needs a proxied egress path (deferred) — fail fast rather
        // than silently allowing or denying everything.
        if let Egress::Allow(_) = spec.egress {
            anyhow::bail!(
                "BWRAP_EGRESS_UNSUPPORTED: the bubblewrap backend cannot enforce an egress \
                 allowlist yet; use egress \"deny\" or route the call via a governed connection"
            );
        }

        let mut args: Vec<String> = [
            "--unshare-user",
            "--unshare-pid",
            "--unshare-ipc",
            "--unshare-uts",
            "--unshare-net",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        args.extend(
            ["--clearenv", "--setenv", "PATH", "/usr/bin:/bin"]
                .into_iter()
                .map(String::from),
        );
        args.extend(Self::scaffold());
        args.extend(
            ["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"]
                .into_iter()
                .map(String::from),
        );

        // Allowlisted ambient env (present on host) → explicit setenv.
        for name in &spec.env_allowlist {
            if let Ok(val) = std::env::var(name) {
                args.extend(["--setenv".into(), name.clone(), val]);
            }
        }
        // praxec-supplied + operator-declared env.
        for (k, v) in &spec.env {
            args.extend(["--setenv".into(), k.clone(), v.clone()]);
        }
        // Read-only binds (e.g. the script body) at their own paths — after the
        // tmpfs so they're visible on top of it.
        for p in &spec.ro_binds {
            let s = p.to_string_lossy().into_owned();
            args.extend(["--ro-bind".into(), s.clone(), s]);
        }
        // Workspace: writable, bound at its own path, and the cwd.
        if let Some(ws) = &spec.workspace {
            let s = ws.to_string_lossy().into_owned();
            args.extend(["--bind".into(), s.clone(), s.clone()]);
            args.extend(["--chdir".into(), s]);
        }
        args.push("--".into());
        args.extend(spec.command.iter().cloned());

        // FMECA §2 — bound process count so a fork-bomb in the sandbox can't
        // exhaust the host. bubblewrap can't cgroup itself, so where a usable
        // systemd user manager exists we run it inside a transient scope with
        // `TasksMax`. Absent that, the wall-clock timeout is the only bound (a
        // documented residual; the agentic tier should prefer the OCI backend).
        let mut cmd = if cgroup_scope_available() {
            let mut c = tokio::process::Command::new("systemd-run");
            c.args([
                "--user",
                "--scope",
                "--quiet",
                "--collect",
                &format!("--property=TasksMax={}", spec.limits.max_pids),
                "--",
                "bwrap",
            ]);
            c
        } else {
            tokio::process::Command::new("bwrap")
        };
        cmd.args(&args);
        // Reap on timeout: when the wall-clock `timeout` below fires it drops the
        // `output()` future; without this the child would detach and keep running
        // (orphaned sandbox). `kill_on_drop` makes the drop terminate the child.
        cmd.kill_on_drop(true);

        let secs = spec.limits.wall_clock_secs.max(1);
        let fut = cmd.output();
        let out = tokio::time::timeout(std::time::Duration::from_secs(secs), fut)
            .await
            .map_err(|_| anyhow::anyhow!("SANDBOX_TIMEOUT: confined run exceeded {secs}s"))?
            .map_err(|e| anyhow::anyhow!("bwrap spawn failed: {e}"))?;
        Ok(SandboxOutput {
            code: out.status.code(),
            success: out.status.success(),
            stdout: out.stdout,
            stderr: out.stderr,
        })
    }
}

/// ADR-0006 — the portable backend (OCI container runtime: docker/podman). A
/// Linux sandbox on every host (VM-backed on macOS/Windows). Unlike bubblewrap
/// it can enforce cgroup limits (`--pids-limit`), and containers start with a
/// clean environment (host secrets are scrubbed by default). Needs a base image
/// with the tools the script uses.
pub struct OciProvider {
    runtime: String,
    image: String,
}

impl OciProvider {
    /// `runtime` is `docker` or `podman`; `image` is the base image scripts run
    /// against.
    pub fn new(runtime: impl Into<String>, image: impl Into<String>) -> Self {
        Self {
            runtime: runtime.into(),
            image: image.into(),
        }
    }

    /// Translate a [`SandboxSpec`] into `<runtime> run …` argv. Pure — this is
    /// the mapping that proves the OS-agnostic spec fits the container model
    /// (ADR-0006 FMECA §5). Fails fast on an egress allowlist (not supported yet)
    /// rather than silently allowing/denying everything.
    pub fn run_args(&self, spec: &SandboxSpec) -> anyhow::Result<Vec<String>> {
        let mut args: Vec<String> = vec!["run".into(), "--rm".into()];
        match &spec.egress {
            Egress::DenyAll => args.extend(["--network".into(), "none".into()]),
            Egress::Allow(_) => anyhow::bail!(
                "OCI_EGRESS_UNSUPPORTED: the container backend cannot enforce an egress \
                 allowlist yet; use egress \"deny\" or route the call via a governed connection"
            ),
        }
        // Containers can cgroup — bound pids (closes part of FMECA §2 that the
        // bubblewrap backend cannot).
        args.extend(["--pids-limit".into(), spec.limits.max_pids.to_string()]);
        // Explicit env (the container env is already clean → scrub by default).
        for (k, v) in &spec.env {
            args.extend(["-e".into(), format!("{k}={v}")]);
        }
        // Allowlisted ambient names → host passthrough (`-e NAME`).
        for name in &spec.env_allowlist {
            args.extend(["-e".into(), name.clone()]);
        }
        // Read-only binds (e.g. the script body) at their own path.
        for p in &spec.ro_binds {
            let s = p.to_string_lossy();
            args.extend(["-v".into(), format!("{s}:{s}:ro")]);
        }
        // Workspace: writable mount + workdir.
        if let Some(ws) = &spec.workspace {
            let s = ws.to_string_lossy().into_owned();
            args.extend(["-v".into(), format!("{s}:{s}"), "-w".into(), s]);
        }
        args.push(self.image.clone());
        args.extend(spec.command.iter().cloned());
        Ok(args)
    }
}

#[async_trait]
impl SandboxProvider for OciProvider {
    fn preflight(&self) -> Preflight {
        // Functional: the runtime's daemon must be reachable (`info` succeeds).
        match std::process::Command::new(&self.runtime)
            .arg("info")
            .output()
        {
            Ok(o) if o.status.success() => Preflight {
                usable: true,
                detail: format!("{} daemon reachable", self.runtime),
                install_hint: None,
            },
            Ok(o) => Preflight {
                usable: false,
                detail: format!(
                    "{} present but daemon not reachable: {}",
                    self.runtime,
                    String::from_utf8_lossy(&o.stderr).trim()
                ),
                install_hint: Some(
                    "start the container runtime (e.g. Docker Desktop, `systemctl start docker`, \
                     or rootless podman)"
                        .into(),
                ),
            },
            Err(e) => Preflight {
                usable: false,
                detail: format!("{} not runnable: {e}", self.runtime),
                install_hint: Some("install a container runtime (Docker or podman)".into()),
            },
        }
    }

    async fn run(&self, spec: &SandboxSpec) -> anyhow::Result<SandboxOutput> {
        let args = self.run_args(spec)?;
        let secs = spec.limits.wall_clock_secs.max(1);
        let mut cmd = tokio::process::Command::new(&self.runtime);
        cmd.args(&args);
        // Reap on timeout (see the bubblewrap provider) — drop must kill the child.
        cmd.kill_on_drop(true);
        let fut = cmd.output();
        let out = tokio::time::timeout(std::time::Duration::from_secs(secs), fut)
            .await
            .map_err(|_| anyhow::anyhow!("SANDBOX_TIMEOUT: confined run exceeded {secs}s"))?
            .map_err(|e| anyhow::anyhow!("{} spawn failed: {e}", self.runtime))?;
        Ok(SandboxOutput {
            code: out.status.code(),
            success: out.status.success(),
            stdout: out.stdout,
            stderr: out.stderr,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn absent_confinement_parses_to_none_unconfined() {
        // The High/High constraint: an unprofiled script is unconfined (today's
        // behavior), not silently denied or confined.
        assert_eq!(
            ConfinementProfile::from_executor_config(&json!({ "kind": "script" })).unwrap(),
            None
        );
        assert_eq!(
            ConfinementProfile::from_executor_config(&json!({ "confinement": null })).unwrap(),
            None
        );
        assert_eq!(
            ConfinementProfile::from_executor_config(&json!({ "confinement": "none" })).unwrap(),
            None
        );
    }

    #[test]
    fn confined_string_parses_to_deny_all_scrubbed() {
        let p = ConfinementProfile::from_executor_config(&json!({ "confinement": "confined" }))
            .unwrap()
            .expect("a profile");
        assert_eq!(p.egress, Egress::DenyAll);
        assert!(p.env_allowlist.is_empty());
    }

    #[test]
    fn object_profile_parses_egress_and_env() {
        let p = ConfinementProfile::from_executor_config(&json!({
            "confinement": { "egress": ["api.internal:443"], "env": ["HOME", "TZ"] }
        }))
        .unwrap()
        .expect("a profile");
        assert_eq!(p.egress, Egress::Allow(vec!["api.internal:443".into()]));
        assert_eq!(p.env_allowlist, vec!["HOME".to_string(), "TZ".to_string()]);
    }

    #[test]
    fn invalid_confinement_fails_fast_not_silent_default() {
        // poka-yoke: a typo'd profile is an error, never a silent fallback to
        // confined or unconfined.
        let err = ConfinementProfile::from_executor_config(&json!({ "confinement": "sandboxed" }))
            .unwrap_err();
        assert!(format!("{err}").contains("INVALID_CONFINEMENT"));
        let err2 =
            ConfinementProfile::from_executor_config(&json!({ "confinement": 42 })).unwrap_err();
        assert!(format!("{err2}").contains("INVALID_CONFINEMENT"));
    }

    #[tokio::test]
    async fn bwrap_egress_allowlist_fails_fast_not_silent() {
        // The bwrap backend can't do selective egress yet — it must refuse, not
        // silently deny-all or allow-all. (Env-independent: no real run.)
        let p = BwrapProvider::new();
        let spec = SandboxSpec {
            workspace: None,
            command: vec!["true".into()],
            ro_binds: vec![],
            env: vec![],
            egress: Egress::Allow(vec!["api:443".into()]),
            env_allowlist: vec![],
            limits: ResourceLimits::default(),
        };
        let err = p.run(&spec).await.unwrap_err();
        assert!(format!("{err}").contains("BWRAP_EGRESS_UNSUPPORTED"));
    }

    #[test]
    fn oci_translates_the_same_spec_to_container_args() {
        // The trait-validation (ADR-0006 FMECA §5): the OS-agnostic SandboxSpec
        // maps cleanly onto a container runtime. Pure — no daemon needed.
        let p = OciProvider::new("docker", "debian:stable-slim");
        let spec = SandboxSpec {
            workspace: Some("/work".into()),
            command: vec!["bash".into(), "/tmp/s.sh".into()],
            ro_binds: vec!["/tmp/s.sh".into()],
            env: vec![("FLAG".into(), "ok".into())],
            egress: Egress::DenyAll,
            env_allowlist: vec!["TZ".into()],
            limits: ResourceLimits {
                wall_clock_secs: 30,
                max_pids: 64,
            },
        };
        let args = p.run_args(&spec).unwrap();
        let joined = args.join(" ");
        assert!(args.starts_with(&["run".to_string(), "--rm".to_string()]));
        assert!(joined.contains("--network none"), "deny egress: {joined}");
        assert!(
            joined.contains("--pids-limit 64"),
            "cgroup pids limit: {joined}"
        );
        assert!(joined.contains("-e FLAG=ok"), "explicit env: {joined}");
        assert!(joined.contains("-e TZ"), "allowlist passthrough: {joined}");
        assert!(
            joined.contains("-v /tmp/s.sh:/tmp/s.sh:ro"),
            "ro bind: {joined}"
        );
        assert!(
            joined.contains("-v /work:/work"),
            "workspace mount: {joined}"
        );
        assert!(joined.contains("-w /work"), "workdir: {joined}");
        assert!(
            joined.ends_with("debian:stable-slim bash /tmp/s.sh"),
            "image + command: {joined}"
        );
    }

    #[tokio::test]
    async fn oci_egress_allowlist_fails_fast() {
        let p = OciProvider::new("docker", "x");
        let spec = SandboxSpec {
            workspace: None,
            command: vec!["true".into()],
            ro_binds: vec![],
            env: vec![],
            egress: Egress::Allow(vec!["api:443".into()]),
            env_allowlist: vec![],
            limits: ResourceLimits::default(),
        };
        let err = p.run(&spec).await.unwrap_err();
        assert!(format!("{err}").contains("OCI_EGRESS_UNSUPPORTED"));
    }

    #[test]
    fn oci_preflight_is_coherent() {
        // Whether or not a daemon is up, preflight is well-formed: usable XOR a
        // remedy. (Env-independent.)
        let pf = OciProvider::new("docker", "debian:stable-slim").preflight();
        assert_eq!(
            pf.usable,
            pf.install_hint.is_none(),
            "usable XOR has a remedy: {pf:?}"
        );
    }

    #[tokio::test]
    async fn bwrap_confines_env_and_passes_explicit_env() {
        let p = BwrapProvider::new();
        let pf = p.preflight();
        if !pf.usable {
            eprintln!(
                "SKIP bwrap confinement test: {} ({:?})",
                pf.detail, pf.install_hint
            );
            return;
        }
        // A host secret in the ambient env must NOT be visible; an explicitly
        // passed var must be.
        // SAFETY: test-local, unique name.
        std::env::set_var("PRAXEC_SANDBOX_SECRET_T", "leaked");
        let spec = SandboxSpec {
            workspace: None,
            command: vec![
                "bash".into(),
                "-c".into(),
                "printf 'sec=%s flag=%s' \"${PRAXEC_SANDBOX_SECRET_T:-none}\" \"${FLAG:-none}\""
                    .into(),
            ],
            ro_binds: vec![],
            env: vec![("FLAG".into(), "ok".into())],
            egress: Egress::DenyAll,
            env_allowlist: vec![],
            limits: ResourceLimits::default(),
        };
        let out = p.run(&spec).await.expect("confined run");
        let s = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.success,
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            s.contains("sec=none"),
            "host secret leaked into sandbox: {s}"
        );
        assert!(s.contains("flag=ok"), "explicit env not passed: {s}");
    }
}
