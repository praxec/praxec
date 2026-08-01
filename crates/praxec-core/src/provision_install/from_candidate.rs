//! Discovery → installer reconciliation (Increment III, Task A4).
//!
//! Discovery ([`crate::tool_catalog`]) surfaces a [`ToolCandidate`] as a
//! **read-only projection** of what a registry offers. When an operator acts on
//! a discovered tool — `praxec tools install <name>` — it must reach the ONE
//! installer, never a second install path
//! (`docs/design/2026-08-01-onboarding-tool-provisioning.md` §3 principle 1).
//!
//! [`from_candidate`] is that bridge: it normalizes a candidate's single
//! [`ToolSource`] to the provider coordinate the installer understands
//! ([`RegistryTool::providers`], keys from [`ProvisionProvider`]), so the
//! resulting [`InstallTarget::Installable`] flows through the same
//! [`super::install`] the curated registry uses. A candidate that is a remote
//! endpoint ([`ToolSource::Url`]) has nothing to fetch: it normalizes to
//! [`InstallTarget::Remote`] — explicit, not an error (a remote MCP server is
//! wired as a url connection, never installed).

use std::collections::BTreeMap;

use crate::registry_v3::RegistryTool;
use crate::tool_catalog::candidate::{ToolCandidate, ToolSource};
use crate::tool_descriptor::ProvisionProvider;

/// The normalized shape of an acted-on discovery candidate. Either it maps to a
/// provider coordinate the ONE installer can provision
/// ([`Installable`](InstallTarget::Installable)), or it is a remote endpoint
/// with nothing to install ([`Remote`](InstallTarget::Remote)) — a first-class,
/// non-error outcome, mirroring the design's "read-only projection that
/// normalizes when acted on".
#[derive(Debug, Clone, PartialEq)]
pub enum InstallTarget {
    /// A tool the installer can provision — a [`RegistryTool`] carrying exactly
    /// one provider coordinate.
    Installable(Box<RegistryTool>),
    /// A remote (transport-`Remote`) tool: nothing to install, wire a url
    /// connection to `url` instead.
    Remote { url: String },
}

/// Normalize a discovered [`ToolCandidate`] to an [`InstallTarget`].
///
/// The single [`ToolSource`] maps to a single provider coordinate:
///
/// | `ToolSource`     | coordinate                       | installer path      |
/// |------------------|----------------------------------|---------------------|
/// | `Image { image }`| `docker: <image>`                | `docker pull`       |
/// | `Repo { url }`   | `release: <url>/releases`        | release binary      |
/// | `Crate { name }` | `cargo: <name>`                  | cargo (emit-only)   |
/// | `Npm { pkg }`    | `npx: <pkg>`                     | npx (provider = A5) |
/// | `Url { url }`    | — (remote)                        | no install          |
///
/// A discovered candidate carries **no version** — the catalog's shape has no
/// version slot. Docker's version-less form is the `latest` tag, so a docker
/// coordinate defaults `version` to `latest` (that is what makes a discovered
/// image installable at all). Every other provider needs a pinned semver it does
/// not have here, so `version` stays `None`: the release URL / cargo crate
/// coordinate is populated (so the map is complete and the ONE installer can
/// resolve it once a version is known), but the **curated registry** remains the
/// path for a pinned release/cargo install.
pub fn from_candidate(candidate: &ToolCandidate) -> InstallTarget {
    let mut providers: BTreeMap<String, String> = BTreeMap::new();
    let mut version: Option<String> = None;

    match &candidate.source {
        ToolSource::Image { image } => {
            providers.insert(
                ProvisionProvider::Docker.as_token().to_string(),
                image.clone(),
            );
            // The one provider whose no-version form is meaningful: `:latest`.
            version = Some("latest".to_string());
        }
        ToolSource::Repo { url } => {
            let releases = format!("{}/releases", url.trim_end_matches('/'));
            providers.insert(ProvisionProvider::Release.as_token().to_string(), releases);
        }
        ToolSource::Crate { name } => {
            providers.insert(
                ProvisionProvider::Cargo.as_token().to_string(),
                name.clone(),
            );
        }
        ToolSource::Npm { pkg } => {
            // Populate the npx coordinate so the map is complete; the npx
            // *provider resolution* is Task A5 — install resolves it once npx
            // lands, and never blocks on it here.
            providers.insert(ProvisionProvider::Npx.as_token().to_string(), pkg.clone());
        }
        ToolSource::Url { url } => {
            return InstallTarget::Remote { url: url.clone() };
        }
    }

    InstallTarget::Installable(Box::new(RegistryTool {
        id: candidate.name.clone(),
        name: candidate.name.clone(),
        description: candidate.description.clone(),
        repo: None,
        command: Some(sanitize_command(&candidate.name)),
        version,
        mcp_registry_id: None,
        providers,
        descriptor: None,
        suggested_workflows: Vec::new(),
    }))
}

/// A conservative command token from a display name: the last path segment (npm
/// scopes carry `/`), with anything outside `[A-Za-z0-9._-]` replaced by `-`.
fn sanitize_command(name: &str) -> String {
    let base = name.rsplit('/').next().unwrap_or(name);
    base.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provision_install::{Host, InstallError, InstallerIo, Provider, resolve_provider};
    use crate::tool_catalog::candidate::{Requires, Transport, TrustTier};
    use std::path::{Path, PathBuf};

    /// A candidate carrying `source`; transport/name are incidental to
    /// normalization (the source is what the installer coordinate derives from).
    fn candidate(name: &str, transport: Transport, source: ToolSource) -> ToolCandidate {
        ToolCandidate {
            name: name.to_string(),
            description: format!("{name} description"),
            transport,
            source,
            verbs: vec![],
            tags: vec![],
            trust_tier: TrustTier::Community,
            requires: Requires::default(),
            provenance: "test-registry".into(),
        }
    }

    fn installable(target: InstallTarget) -> RegistryTool {
        match target {
            InstallTarget::Installable(tool) => *tool,
            other => panic!("expected Installable, got {other:?}"),
        }
    }

    /// A fake IO whose only meaningful read is `which` — enough to drive
    /// `resolve_provider` (a pure, non-mutating resolve).
    struct WhichIo {
        has_docker: bool,
    }
    impl InstallerIo for WhichIo {
        fn http_get(&self, url: &str) -> Result<Vec<u8>, InstallError> {
            Err(InstallError::Io(format!("unused {url}")))
        }
        fn place_executable(
            &self,
            _dir: &Path,
            _name: &str,
            _bytes: &[u8],
        ) -> Result<PathBuf, InstallError> {
            unreachable!("resolve does not place")
        }
        fn installed_version(&self, _dir: &Path, _name: &str) -> Option<String> {
            None
        }
        fn bin_dir(&self) -> Result<PathBuf, InstallError> {
            Ok(PathBuf::from("/fake/bin"))
        }
        fn which(&self, cmd: &str) -> bool {
            cmd == "docker" && self.has_docker
        }
        fn docker_pull(&self, _image_ref: &str) -> Result<(), InstallError> {
            unreachable!("resolve does not pull")
        }
    }

    fn unmapped_host() -> Host {
        // riscv64 has no published release triple, so the chain falls past the
        // release arm to docker/cargo — isolating the coordinate under test.
        Host {
            os: "linux".into(),
            arch: "riscv64".into(),
        }
    }

    #[test]
    fn image_normalizes_to_a_docker_coordinate() {
        let tool = installable(from_candidate(&candidate(
            "acme-mcp",
            Transport::Docker,
            ToolSource::Image {
                image: "ghcr.io/acme/mcp".into(),
            },
        )));
        assert_eq!(
            tool.providers.get("docker").map(String::as_str),
            Some("ghcr.io/acme/mcp")
        );
    }

    #[test]
    fn image_target_is_docker_resolvable() {
        // "docker-resolvable": the chain, with a docker daemon present, picks
        // Docker and the plan pulls `<image>:latest` (the version-less form).
        let tool = installable(from_candidate(&candidate(
            "acme-mcp",
            Transport::Docker,
            ToolSource::Image {
                image: "ghcr.io/acme/mcp".into(),
            },
        )));
        let plan = resolve_provider(&tool, &unmapped_host(), &WhichIo { has_docker: true })
            .expect("docker resolves");
        assert_eq!(plan.provider, Provider::Docker);
        assert_eq!(plan.command, "docker pull ghcr.io/acme/mcp:latest");
    }

    #[test]
    fn repo_normalizes_to_a_release_coordinate_at_the_releases_page() {
        let tool = installable(from_candidate(&candidate(
            "corpus",
            Transport::Stdio,
            ToolSource::Repo {
                url: "https://github.com/praxec/corpus".into(),
            },
        )));
        assert_eq!(
            tool.providers.get("release").map(String::as_str),
            Some("https://github.com/praxec/corpus/releases")
        );
    }

    #[test]
    fn repo_release_coordinate_does_not_double_the_slash() {
        // A trailing slash on the repo url must not yield `…//releases`.
        let tool = installable(from_candidate(&candidate(
            "corpus",
            Transport::Stdio,
            ToolSource::Repo {
                url: "https://github.com/praxec/corpus/".into(),
            },
        )));
        assert_eq!(
            tool.providers.get("release").map(String::as_str),
            Some("https://github.com/praxec/corpus/releases")
        );
    }

    #[test]
    fn crate_normalizes_to_a_cargo_coordinate() {
        let tool = installable(from_candidate(&candidate(
            "cpm-planner",
            Transport::Stdio,
            ToolSource::Crate {
                name: "cpm-planner".into(),
            },
        )));
        assert_eq!(
            tool.providers.get("cargo").map(String::as_str),
            Some("cpm-planner")
        );
    }

    #[test]
    fn crate_target_resolves_to_emit_only_cargo() {
        // Cargo is emit-only: with no docker/release, the chain resolves Cargo
        // and (version-less) emits the bare `cargo install <crate>` command.
        let tool = installable(from_candidate(&candidate(
            "cpm-planner",
            Transport::Stdio,
            ToolSource::Crate {
                name: "cpm-planner".into(),
            },
        )));
        let plan = resolve_provider(&tool, &unmapped_host(), &WhichIo { has_docker: false })
            .expect("cargo resolves");
        assert_eq!(plan.provider, Provider::Cargo);
        assert_eq!(plan.command, "cargo install cpm-planner");
    }

    #[test]
    fn npm_normalizes_to_an_npx_coordinate() {
        // A5 adds the npx PROVIDER resolution; here only the coordinate is
        // populated so the map is complete.
        let tool = installable(from_candidate(&candidate(
            "browser-mcp",
            Transport::Stdio,
            ToolSource::Npm {
                pkg: "@playwright/mcp".into(),
            },
        )));
        assert_eq!(
            tool.providers.get("npx").map(String::as_str),
            Some("@playwright/mcp")
        );
    }

    #[test]
    fn url_normalizes_to_a_remote_no_install_target() {
        let target = from_candidate(&candidate(
            "remote-mcp",
            Transport::Remote,
            ToolSource::Url {
                url: "https://mcp.example.com/sse".into(),
            },
        ));
        assert_eq!(
            target,
            InstallTarget::Remote {
                url: "https://mcp.example.com/sse".into()
            }
        );
    }

    #[test]
    fn scoped_npm_name_sanitizes_to_a_bare_command_token() {
        // A display name that is itself a scoped pkg still yields a clean command.
        let tool = installable(from_candidate(&candidate(
            "@acme/browser-mcp",
            Transport::Stdio,
            ToolSource::Npm {
                pkg: "@acme/browser-mcp".into(),
            },
        )));
        assert_eq!(tool.command.as_deref(), Some("browser-mcp"));
    }
}
