/// A connection entry from the gateway config.
pub struct Connection {
    pub kind: String,
    pub command: String,
    /// `optional: true` — a premium/add-on connection whose absence is expected
    /// and benign (the open-source flow skips it). A missing optional command is
    /// reported as [`ProvisionReport::missing_optional`], never as a required gap.
    pub optional: bool,
}

/// Resolved gateway config for provisioning.
pub struct Config {
    pub connections: Vec<Connection>,
}

/// Report of PATH-resolvable MCP commands: which binaries are present
/// vs missing.
pub struct ProvisionReport {
    pub present: Vec<String>,
    /// REQUIRED connections whose command is not installed — a real gap to fix.
    pub missing: Vec<String>,
    /// OPTIONAL (premium/add-on) connections whose command is not installed —
    /// expected and benign: the gateway and open-source packs run without them.
    pub missing_optional: Vec<String>,
}

/// Enumerate every `kind: mcp` connection, check whether its `command`
/// binary is resolvable on PATH, and return a typed report.
pub fn detect(config: &Config) -> ProvisionReport {
    detect_with(
        config,
        praxec_core::provision_install::managed_bin_dir().as_deref(),
    )
}

/// The [`detect`] core, with the praxec-managed bin dir passed in so unit tests
/// inject a tempdir instead of touching the real `~/.config`. A command counts
/// as PRESENT if it resolves on PATH OR a binary for it lives in `managed_dir`
/// (where `praxec tools install` places release binaries) — so a
/// managed-installed tool reads as `ok`, not `missing`.
fn detect_with(config: &Config, managed_dir: Option<&std::path::Path>) -> ProvisionReport {
    let mut present = Vec::new();
    let mut missing = Vec::new();
    let mut missing_optional = Vec::new();

    for conn in &config.connections {
        if conn.kind != "mcp" {
            continue;
        }
        // Resolve exactly as a real spawn would: `command_resolves` searches the
        // ambient PATH AND the dirs praxec prepends to the child PATH (managed
        // bin, ~/.cargo/bin, ~/.local/bin), `.exe`-aware — so a `cargo install`'d
        // tool reads as PRESENT here, not a false "unknown tool". `managed_dir`
        // is the test-injected override of the managed bin dir.
        let resolvable = praxec_core::provision_install::command_resolves(&conn.command)
            || in_managed_dir(managed_dir, &conn.command);
        if resolvable {
            present.push(conn.command.clone());
        } else if conn.optional {
            // A premium/add-on tool the operator has not installed — expected and
            // benign; never a required gap.
            missing_optional.push(conn.command.clone());
        } else {
            missing.push(conn.command.clone());
        }
    }

    ProvisionReport {
        present,
        missing,
        missing_optional,
    }
}

/// Does a spawnable binary for `command` exist in the managed bin dir? Delegates
/// to the ONE `.exe`-aware managed-bin predicate
/// ([`praxec_core::provision_install::managed_binary_in`]) so the bare-name /
/// `.exe` rule never drifts from currency's or the installer's copy.
fn in_managed_dir(managed_dir: Option<&std::path::Path>, command: &str) -> bool {
    managed_dir
        .and_then(|dir| praxec_core::provision_install::managed_binary_in(dir, command))
        .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A required `kind: mcp` connection with `command`.
    fn mcp(command: &str) -> Connection {
        Connection {
            kind: "mcp".into(),
            command: command.into(),
            optional: false,
        }
    }

    #[test]
    fn missing_command_classified_as_missing() {
        let config = Config {
            connections: vec![mcp("nonexistent_command_xyz")],
        };

        let report = detect(&config);

        assert_eq!(report.missing, vec!["nonexistent_command_xyz"]);
        assert!(report.missing_optional.is_empty());
    }

    #[test]
    fn present_command_classified_as_present() {
        let config = Config {
            connections: vec![mcp("cargo")],
        };

        let report = detect(&config);

        assert_eq!(report.present, vec!["cargo"]);
    }

    #[test]
    fn mixed_present_and_missing_mcp_commands_are_classified() {
        let config = Config {
            connections: vec![mcp("nonexistent_command_xyz"), mcp("cargo")],
        };

        let report = detect(&config);

        assert_eq!(report.present, vec!["cargo"]);
    }

    #[test]
    fn non_mcp_connection_is_skipped() {
        let config = Config {
            connections: vec![Connection {
                kind: "stdio".into(),
                command: "cargo".into(),
                optional: false,
            }],
        };

        let report = detect(&config);

        assert!(report.present.is_empty() && report.missing.is_empty());
    }

    #[test]
    fn command_only_in_managed_bin_dir_is_present() {
        // A tool `praxec tools install`-ed into the managed bin dir but NOT on
        // PATH must read as present, not missing — detect consults the managed
        // dir (injected here) as well as PATH.
        let dir = tempfile::tempdir().unwrap();
        let cmd = "managed_only_tool_xyz";
        let file_name = if cfg!(windows) {
            format!("{cmd}.exe")
        } else {
            cmd.to_string()
        };
        std::fs::write(dir.path().join(&file_name), b"#!/bin/sh\n").unwrap();

        let config = Config {
            connections: vec![mcp(cmd)],
        };

        let report = detect_with(&config, Some(dir.path()));

        assert_eq!(report.present, vec![cmd]);
        assert!(report.missing.is_empty());
    }

    #[test]
    fn command_absent_from_path_and_managed_dir_is_missing() {
        let dir = tempfile::tempdir().unwrap(); // empty managed dir
        let config = Config {
            connections: vec![mcp("nonexistent_command_xyz")],
        };

        let report = detect_with(&config, Some(dir.path()));

        assert_eq!(report.missing, vec!["nonexistent_command_xyz"]);
        assert!(report.present.is_empty());
    }

    // Open-core boundary: an ABSENT OPTIONAL connection is classified as
    // `missing_optional` (expected/benign), NOT `missing` (a required gap). This
    // is what keeps a premium/add-on tool from reading as a problem in doctor.
    #[test]
    fn absent_optional_connection_is_missing_optional_not_missing() {
        let dir = tempfile::tempdir().unwrap(); // empty managed dir
        let config = Config {
            connections: vec![Connection {
                kind: "mcp".into(),
                command: "nonexistent_premium_tool_xyz".into(),
                optional: true,
            }],
        };

        let report = detect_with(&config, Some(dir.path()));

        assert_eq!(
            report.missing_optional,
            vec!["nonexistent_premium_tool_xyz"]
        );
        assert!(
            report.missing.is_empty(),
            "an optional tool must never be a required gap"
        );
    }
}
