//! The onboarding **provider chain** (Release → Docker → Cargo) + the docker
//! provider, layered over the release provider ([`super::install_release`]).
//!
//! Design: `docs/design/2026-08-01-onboarding-tool-provisioning.md` §3-4 and the
//! §8 amendment to ADR-0013. For the **onboarding/init path specifically** the
//! effective order is release-binary → docker → cargo — lowest friction for a
//! fresh machine with no Docker daemon (§3 principle 2). This re-weights
//! ADR-0013's global docker-default *for this path only*; both providers stay
//! first-class.
//!
//! Two entry points, both consent-gated (§3 principle 3 — no silent install):
//! - [`resolve_provider`] reports the highest-preference **available** provider
//!   as an [`InstallPlan`] *without touching anything* — doctor's "offer".
//! - [`install`] performs the chosen provider under [`Consent::Granted`], or
//!   returns the plan (as [`InstallOutcome::Offered`]) under
//!   [`Consent::OfferOnly`].
//!
//! Availability rules:
//! - **Release** — available when the tool has `command` + `version` + a
//!   `release` provider URL *and* the host `(os, arch)` maps to a published
//!   triple ([`super::resolve_target`]). Preferred whenever available.
//! - **Docker** — available only when `io.which("docker")` is true and the tool
//!   has a `docker` image + `version`. Install = `docker pull <image>:<version>`.
//! - **Npx** — available when `io.which("npx")` is true and the tool has an
//!   `npx` coordinate (the package name). "Install" is a **no-op**: an
//!   npm-distributed stdio server is nothing to download or place — npx fetches
//!   it on run, and its connection wires `{command: npx, args: ["-y", <pkg>]}`.
//!   Ordered before cargo (no toolchain needed); never a source build.
//! - **Cargo** — last-resort source path, **emit-only**: even under
//!   `Consent::Granted` it only *returns* the `cargo install <crate>` command;
//!   it never shells out to cargo (that source-build is the exact Windows pain
//!   this whole feature removes — §4 point 4).

use std::path::PathBuf;

use crate::registry_v3::RegistryTool;
use crate::tool_descriptor::ProvisionProvider;

use super::InstallerIo;
use super::{Host, InstallError, InstallOutcome, asset_name, install_release, resolve_target};

/// Which provider the chain resolved to. Ordered by onboarding preference
/// (`Release` highest); shared verbatim with the doctor/init surfaces (T4/T5).
///
/// `Npx` sits before `Cargo`: an npm-distributed stdio MCP server needs **no
/// toolchain** (npx fetches it on run), so it beats the last-resort source
/// build — but still after release/docker, which are pinned/reproducible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Release,
    Docker,
    Npx,
    Cargo,
}

/// The operator's consent for a given [`install`] call. `OfferOnly` is the
/// default posture (report the plan, mutate nothing); `Granted` is the explicit
/// `--fix` / `--install-tools` / `--yes` consent (§3 principle 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consent {
    OfferOnly,
    Granted,
}

/// A resolved-but-not-yet-performed action: the provider the chain picked and
/// the exact human-readable command (a `docker pull …`, a release asset URL, or
/// a `cargo install …`). Produced by [`resolve_provider`] with zero mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPlan {
    pub provider: Provider,
    /// The human-readable action, e.g. `docker pull ghcr.io/praxec/cpm-planner:0.0.1`,
    /// the release asset URL, or `cargo install cpm-planner --version 0.0.1`.
    pub command: String,
}

/// The release plan for this tool + host, or `None` when the tool lacks the
/// required release coordinates or the host has no published triple. Pure — no
/// IO, so it never mutates and never touches the network.
fn release_plan(tool: &RegistryTool, host: &Host) -> Option<InstallPlan> {
    let command = tool.command.as_deref()?;
    let version = tool.version.as_deref()?;
    let page = tool.providers.get(ProvisionProvider::Release.as_token())?;
    let (triple, ext) = resolve_target(&host.os, &host.arch)?;
    let asset = asset_name(command, triple, ext);
    let page = page.trim_end_matches('/');
    Some(InstallPlan {
        provider: Provider::Release,
        command: format!("{page}/download/v{version}/{asset}"),
    })
}

/// The docker plan, gated on `io.which("docker")` (the daemon/CLI must be
/// present) and the tool having a `docker` image + `version`. `which` is a pure
/// read, so calling it here does not mutate.
fn docker_plan(tool: &RegistryTool, io: &dyn InstallerIo) -> Option<InstallPlan> {
    let image = tool.providers.get(ProvisionProvider::Docker.as_token())?;
    let version = tool.version.as_deref()?;
    if !io.which("docker") {
        return None;
    }
    Some(InstallPlan {
        provider: Provider::Docker,
        command: format!("docker pull {image}:{version}"),
    })
}

/// The npx plan, gated on `io.which("npx")` and the tool having an `npx`
/// coordinate (the package name). An npm-distributed stdio MCP server is **not**
/// downloaded or placed — npx fetches it on run — so the plan's command is the
/// connection form `npx -y <pkg>`. `which` is a pure read (no mutation).
/// Ordered before cargo: it needs no toolchain, never a source build.
fn npx_plan(tool: &RegistryTool, io: &dyn InstallerIo) -> Option<InstallPlan> {
    let pkg = tool.providers.get(ProvisionProvider::Npx.as_token())?;
    if !io.which("npx") {
        return None;
    }
    Some(InstallPlan {
        provider: Provider::Npx,
        command: format!("npx -y {pkg}"),
    })
}

/// The cargo plan — the last-resort source path. Emit-only: it names the
/// command but is never executed by this module (§4 point 4).
fn cargo_plan(tool: &RegistryTool) -> Option<InstallPlan> {
    let crate_name = tool.providers.get(ProvisionProvider::Cargo.as_token())?;
    let command = match tool.version.as_deref() {
        Some(v) => format!("cargo install {crate_name} --version {v}"),
        None => format!("cargo install {crate_name}"),
    };
    Some(InstallPlan {
        provider: Provider::Cargo,
        command,
    })
}

/// Resolve the highest-preference **available** provider for `tool` on `host`
/// WITHOUT installing (doctor's "offer"). Onboarding order: Release → Docker →
/// Npx → Cargo (§3 principle 2). Returns `None` when no provider resolves.
///
/// Guaranteed non-mutating: the only IO it performs is `io.which` (a read).
pub fn resolve_provider(
    tool: &RegistryTool,
    host: &Host,
    io: &dyn InstallerIo,
) -> Option<InstallPlan> {
    release_plan(tool, host)
        .or_else(|| docker_plan(tool, io))
        .or_else(|| npx_plan(tool, io))
        .or_else(|| cargo_plan(tool))
}

/// Install `tool` via the provider chain, consent-gated.
///
/// - [`Consent::OfferOnly`] mutates nothing and returns the plan as
///   [`InstallOutcome::Offered`].
/// - [`Consent::Granted`] performs the chosen provider: Release delegates to
///   [`super::install_release`] (download + checksum-verify + place); Docker
///   runs `docker pull`; Npx is a **no-op success** (nothing to place — npx
///   fetches on run — returned as `NoInstallNeeded` surfacing `npx -y <pkg>`);
///   Cargo remains **emit-only** (returns the command as `Offered`, never
///   shells out).
///
/// Fails fast with [`InstallError::NoProvider`] (naming the tool) when the chain
/// resolves nothing.
pub fn install(
    tool: &RegistryTool,
    host: &Host,
    consent: Consent,
    io: &dyn InstallerIo,
) -> Result<InstallOutcome, InstallError> {
    let plan = resolve_provider(tool, host, io).ok_or_else(|| InstallError::NoProvider {
        tool: tool.id.clone(),
    })?;

    // OfferOnly: report the plan, mutate nothing.
    if consent == Consent::OfferOnly {
        return Ok(InstallOutcome::Offered {
            provider: plan.provider,
            command: plan.command,
        });
    }

    // Granted: perform the chosen provider.
    match plan.provider {
        Provider::Release => install_release(tool, host, io),
        Provider::Docker => {
            let image = tool
                .providers
                .get(ProvisionProvider::Docker.as_token())
                .ok_or_else(|| InstallError::MissingField {
                    tool: tool.id.clone(),
                    field: "providers.docker",
                })?;
            let version = tool
                .version
                .as_deref()
                .ok_or_else(|| InstallError::MissingField {
                    tool: tool.id.clone(),
                    field: "version",
                })?;
            let image_ref = format!("{image}:{version}");
            io.docker_pull(&image_ref)?;
            Ok(InstallOutcome::Installed {
                // Docker connections spawn `docker run <image_ref>`; there is no
                // on-disk binary path, so the placed path is the image ref
                // marker. Wiring the connection body is T5/T6.
                path: PathBuf::from(format!("docker://{image_ref}")),
                version: version.to_string(),
            })
        }
        // Npx is a no-op success even under Granted: an npm-distributed stdio
        // server is nothing to download or place — npx fetches it on run. The
        // tool is "ready" because its connection (`npx -y <pkg>`, surfaced in
        // the reason) will fetch it on demand. Never a source build.
        Provider::Npx => Ok(InstallOutcome::NoInstallNeeded {
            reason: format!(
                "npm-distributed tool runs on demand via `{}` — nothing to download or place",
                plan.command
            ),
        }),
        // Cargo is emit-only even under Granted: never trigger a source build.
        Provider::Cargo => Ok(InstallOutcome::Offered {
            provider: Provider::Cargo,
            command: plan.command,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::path::Path;

    // ── fake IO recording every host-touching call ───────────────────────────
    #[derive(Default)]
    struct FakeIo {
        responses: HashMap<String, Vec<u8>>,
        installed: Option<String>,
        bin_dir: PathBuf,
        has_docker: bool,
        has_npx: bool,
        writes: RefCell<Vec<(PathBuf, String, usize)>>,
        pulls: RefCell<Vec<String>>,
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
        fn which(&self, cmd: &str) -> bool {
            (cmd == "docker" && self.has_docker) || (cmd == "npx" && self.has_npx)
        }
        fn docker_pull(&self, image_ref: &str) -> Result<(), InstallError> {
            self.pulls.borrow_mut().push(image_ref.to_string());
            Ok(())
        }
    }

    // ── fixture builders ─────────────────────────────────────────────────────
    fn sha256_hex(bytes: &[u8]) -> String {
        let digest = Sha256::digest(bytes);
        let mut s = String::with_capacity(digest.len() * 2);
        for b in digest {
            s.push_str(&format!("{b:02x}"));
        }
        s
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

    /// A tool with an explicit provider set (only the given keys present).
    fn tool_with(command: &str, version: &str, providers: &[(&str, &str)]) -> RegistryTool {
        RegistryTool {
            id: format!("{command}-tool"),
            name: command.to_string(),
            description: String::new(),
            repo: None,
            command: Some(command.to_string()),
            version: Some(version.to_string()),
            mcp_registry_id: None,
            providers: providers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            descriptor: None,
            suggested_workflows: Vec::new(),
        }
    }

    fn linux_host() -> Host {
        Host {
            os: "linux".into(),
            arch: "x86_64".into(),
        }
    }

    /// A host whose arch has no published release triple (`resolve_target` None)
    /// — forces the chain past the release arm.
    fn unmapped_host() -> Host {
        Host {
            os: "linux".into(),
            arch: "riscv64".into(),
        }
    }

    /// Register the release asset + checksum responses so a Release install of
    /// `tool` on a linux/x86_64 host succeeds.
    fn register_linux_release(io: &mut FakeIo, page: &str, command: &str, version: &str) {
        let asset = format!("{command}-x86_64-unknown-linux-gnu.tar.gz");
        let bytes = make_targz(command, b"#!/bin/sh\n");
        io.responses
            .insert(format!("{page}/download/v{version}/{asset}"), bytes.clone());
        io.responses.insert(
            format!("{page}/download/v{version}/checksums.sha256"),
            format!("{}  {asset}\n", sha256_hex(&bytes)).into_bytes(),
        );
    }

    // ── contract 1: Release preferred over an available docker ───────────────
    #[test]
    fn release_is_chosen_over_docker_when_both_available() {
        let page = "https://github.com/praxec/cpm-planner/releases";
        let tool = tool_with(
            "cpm-planner",
            "0.0.2",
            &[("release", page), ("docker", "ghcr.io/praxec/cpm-planner")],
        );
        let mut io = FakeIo {
            bin_dir: "/fake/bin".into(),
            has_docker: true,
            ..Default::default()
        };
        register_linux_release(&mut io, page, "cpm-planner", "0.0.2");

        // resolve prefers Release even with docker present + running.
        let plan = resolve_provider(&tool, &linux_host(), &io).unwrap();
        assert_eq!(plan.provider, Provider::Release);

        // Granted install goes the release route: a binary is placed, docker
        // is never pulled.
        let out = install(&tool, &linux_host(), Consent::Granted, &io).unwrap();
        assert!(
            matches!(out, InstallOutcome::Installed { .. }),
            "got {out:?}"
        );
        assert_eq!(io.pulls.borrow().len(), 0, "docker_pull must not run");
        assert_eq!(io.writes.borrow().len(), 1, "release binary placed");
    }

    // ── contract 2: docker chosen when no release asset resolves ─────────────
    #[test]
    fn docker_is_chosen_when_no_release_asset_resolves() {
        let tool = tool_with(
            "cpm-planner",
            "0.0.2",
            &[
                ("release", "https://github.com/praxec/cpm-planner/releases"),
                ("docker", "ghcr.io/praxec/cpm-planner"),
            ],
        );
        // Unmapped host → release_plan is None → chain falls to docker.
        let io = FakeIo {
            bin_dir: "/fake/bin".into(),
            has_docker: true,
            ..Default::default()
        };

        let plan = resolve_provider(&tool, &unmapped_host(), &io).unwrap();
        assert_eq!(plan.provider, Provider::Docker);
        assert_eq!(plan.command, "docker pull ghcr.io/praxec/cpm-planner:0.0.2");

        let out = install(&tool, &unmapped_host(), Consent::Granted, &io).unwrap();
        assert_eq!(
            io.pulls.borrow().as_slice(),
            &["ghcr.io/praxec/cpm-planner:0.0.2".to_string()],
            "docker pull invoked with image:version"
        );
        assert_eq!(
            io.writes.borrow().len(),
            0,
            "no on-disk placement for docker"
        );
        assert!(matches!(out, InstallOutcome::Installed { .. }));
    }

    // ── contract 3: docker skipped when which("docker") is false ─────────────
    #[test]
    fn docker_skipped_when_daemon_absent_falls_through() {
        // Only a docker provider, unmapped host, and no docker daemon → nothing
        // resolves (release absent for host, docker gated out, no cargo).
        let tool = tool_with("cpm-planner", "0.0.2", &[("docker", "ghcr.io/x/y")]);
        let io = FakeIo {
            bin_dir: "/fake/bin".into(),
            has_docker: false,
            ..Default::default()
        };
        assert!(
            resolve_provider(&tool, &unmapped_host(), &io).is_none(),
            "docker must be skipped without a daemon and fall through to None"
        );
        let err = install(&tool, &unmapped_host(), Consent::Granted, &io).unwrap_err();
        assert!(matches!(err, InstallError::NoProvider { .. }));
        assert!(err.to_string().contains("cpm-planner-tool"));
    }

    // ── contract 4: resolve_provider reports without mutation ────────────────
    #[test]
    fn resolve_provider_reports_the_plan_without_mutating() {
        let page = "https://github.com/praxec/cpm-planner/releases";
        let tool = tool_with(
            "cpm-planner",
            "0.0.2",
            &[("release", page), ("docker", "ghcr.io/x/y")],
        );
        let io = FakeIo {
            bin_dir: "/fake/bin".into(),
            has_docker: true,
            ..Default::default()
        };
        let plan = resolve_provider(&tool, &linux_host(), &io).unwrap();
        assert_eq!(plan.provider, Provider::Release);
        assert_eq!(
            plan.command,
            format!("{page}/download/v0.0.2/cpm-planner-x86_64-unknown-linux-gnu.tar.gz")
        );
        // Zero mutation: no pull, no write, no download.
        assert_eq!(io.pulls.borrow().len(), 0);
        assert_eq!(io.writes.borrow().len(), 0);
    }

    // ── contract 5: OfferOnly never mutates; Granted performs ────────────────
    #[test]
    fn offer_only_is_inert_while_granted_performs_the_pull() {
        let tool = tool_with(
            "cpm-planner",
            "0.0.2",
            &[("docker", "ghcr.io/praxec/cpm-planner")],
        );
        let io = FakeIo {
            bin_dir: "/fake/bin".into(),
            has_docker: true,
            ..Default::default()
        };

        // OfferOnly → plan reported, nothing pulled/written.
        let offered = install(&tool, &unmapped_host(), Consent::OfferOnly, &io).unwrap();
        assert_eq!(
            offered,
            InstallOutcome::Offered {
                provider: Provider::Docker,
                command: "docker pull ghcr.io/praxec/cpm-planner:0.0.2".into(),
            }
        );
        assert_eq!(io.pulls.borrow().len(), 0, "OfferOnly must not pull");
        assert_eq!(io.writes.borrow().len(), 0);

        // Granted → the pull runs.
        install(&tool, &unmapped_host(), Consent::Granted, &io).unwrap();
        assert_eq!(io.pulls.borrow().len(), 1, "Granted performs the pull");
    }

    // ── contract 6: cargo is emit-only — never shells out ────────────────────
    #[test]
    fn cargo_is_emit_only_even_under_granted() {
        let tool = tool_with("cpm-planner", "0.0.2", &[("cargo", "cpm-planner")]);
        let io = FakeIo {
            bin_dir: "/fake/bin".into(),
            has_docker: true, // docker present but tool has no docker image
            ..Default::default()
        };

        let plan = resolve_provider(&tool, &unmapped_host(), &io).unwrap();
        assert_eq!(plan.provider, Provider::Cargo);
        assert!(
            plan.command.contains("cargo install cpm-planner"),
            "plan names the cargo command: {}",
            plan.command
        );

        // Granted must NOT run cargo: emit-only. Nothing was pulled or written,
        // and the outcome is the emitted command.
        let out = install(&tool, &unmapped_host(), Consent::Granted, &io).unwrap();
        assert!(
            matches!(&out, InstallOutcome::Offered { provider: Provider::Cargo, command }
                if command.contains("cargo install cpm-planner")),
            "got {out:?}"
        );
        assert_eq!(io.pulls.borrow().len(), 0, "cargo must not pull docker");
        assert_eq!(io.writes.borrow().len(), 0, "cargo must not place a binary");
    }

    // ── contract 7: npx resolves + is a no-op success wiring `npx -y <pkg>` ───
    #[test]
    fn npx_candidate_resolves_and_installs_as_a_no_op() {
        let tool = tool_with("browser-mcp", "0.0.2", &[("npx", "@playwright/mcp")]);
        let io = FakeIo {
            bin_dir: "/fake/bin".into(),
            has_npx: true,
            ..Default::default()
        };

        // resolve picks Npx; the plan surfaces the `npx -y <pkg>` connection form.
        let plan = resolve_provider(&tool, &unmapped_host(), &io).unwrap();
        assert_eq!(plan.provider, Provider::Npx);
        assert_eq!(plan.command, "npx -y @playwright/mcp");

        // Granted install is a no-op success: nothing downloaded, nothing placed,
        // and the reported outcome surfaces the `npx -y <pkg>` command.
        let out = install(&tool, &unmapped_host(), Consent::Granted, &io).unwrap();
        match &out {
            InstallOutcome::NoInstallNeeded { reason } => {
                assert!(
                    reason.contains("npx -y @playwright/mcp"),
                    "outcome surfaces the connection command: {reason}"
                );
            }
            other => panic!("expected NoInstallNeeded, got {other:?}"),
        }
        assert_eq!(io.writes.borrow().len(), 0, "npx places no binary");
        assert_eq!(io.pulls.borrow().len(), 0, "npx pulls no image");
        assert!(io.responses.is_empty(), "no http_get responses were needed");
    }

    // ── contract 8: npx gated on which("npx") — absent → falls through ────────
    #[test]
    fn npx_absent_falls_through_to_cargo() {
        // Both an npx and a cargo coordinate; npx unavailable → cargo wins.
        let tool = tool_with(
            "browser-mcp",
            "0.0.2",
            &[("npx", "@playwright/mcp"), ("cargo", "browser-mcp")],
        );
        let io = FakeIo {
            bin_dir: "/fake/bin".into(),
            has_npx: false,
            ..Default::default()
        };
        let plan = resolve_provider(&tool, &unmapped_host(), &io).unwrap();
        assert_eq!(plan.provider, Provider::Cargo, "npx gated out → cargo");

        // With only an npx coordinate and no npx → nothing resolves.
        let npx_only = tool_with("browser-mcp", "0.0.2", &[("npx", "@playwright/mcp")]);
        assert!(
            resolve_provider(&npx_only, &unmapped_host(), &io).is_none(),
            "no npx and no other provider → None"
        );
    }

    // ── contract 9: chain order — release wins over npx; npx wins over cargo ──
    #[test]
    fn release_beats_npx_and_npx_beats_cargo() {
        let page = "https://github.com/praxec/browser-mcp/releases";
        // Release + npx both available on a mapped host → Release wins.
        let both = tool_with(
            "browser-mcp",
            "0.0.2",
            &[("release", page), ("npx", "@playwright/mcp")],
        );
        let io = FakeIo {
            bin_dir: "/fake/bin".into(),
            has_npx: true,
            ..Default::default()
        };
        let plan = resolve_provider(&both, &linux_host(), &io).unwrap();
        assert_eq!(plan.provider, Provider::Release, "release beats npx");

        // Only npx + cargo (no release/docker) → Npx beats the source build.
        let npx_cargo = tool_with(
            "browser-mcp",
            "0.0.2",
            &[("npx", "@playwright/mcp"), ("cargo", "browser-mcp")],
        );
        let plan = resolve_provider(&npx_cargo, &unmapped_host(), &io).unwrap();
        assert_eq!(plan.provider, Provider::Npx, "npx beats cargo");
    }

    // ── contract 10: npx OfferOnly reports the plan, mutates nothing ──────────
    #[test]
    fn npx_offer_only_reports_the_plan_without_mutating() {
        let tool = tool_with("browser-mcp", "0.0.2", &[("npx", "@playwright/mcp")]);
        let io = FakeIo {
            bin_dir: "/fake/bin".into(),
            has_npx: true,
            ..Default::default()
        };
        let offered = install(&tool, &unmapped_host(), Consent::OfferOnly, &io).unwrap();
        assert_eq!(
            offered,
            InstallOutcome::Offered {
                provider: Provider::Npx,
                command: "npx -y @playwright/mcp".into(),
            }
        );
        assert_eq!(io.writes.borrow().len(), 0);
        assert_eq!(io.pulls.borrow().len(), 0);
    }
}
