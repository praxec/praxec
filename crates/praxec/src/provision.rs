/// A connection entry from the gateway config.
pub struct Connection {
    pub kind: String,
    pub command: String,
}

/// Resolved gateway config for provisioning.
pub struct Config {
    pub connections: Vec<Connection>,
}

/// Report of PATH-resolvable MCP commands: which binaries are present
/// vs missing.
pub struct ProvisionReport {
    pub present: Vec<String>,
    pub missing: Vec<String>,
}

/// Enumerate every `kind: mcp` connection, check whether its `command`
/// binary is resolvable on PATH, and return a typed report.
pub fn detect(config: &Config) -> ProvisionReport {
    let mut present = Vec::new();
    let mut missing = Vec::new();

    for conn in &config.connections {
        if conn.kind != "mcp" {
            continue;
        }
        if which::which(&conn.command).is_ok() {
            present.push(conn.command.clone());
        } else {
            missing.push(conn.command.clone());
        }
    }

    ProvisionReport { present, missing }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_command_classified_as_missing() {
        let config = Config {
            connections: vec![Connection {
                kind: "mcp".into(),
                command: "nonexistent_command_xyz".into(),
            }],
        };

        let report = detect(&config);

        assert_eq!(report.missing, vec!["nonexistent_command_xyz"]);
    }

    #[test]
    fn present_command_classified_as_present() {
        let config = Config {
            connections: vec![Connection {
                kind: "mcp".into(),
                command: "cargo".into(),
            }],
        };

        let report = detect(&config);

        assert_eq!(report.present, vec!["cargo"]);
    }

    #[test]
    fn mixed_present_and_missing_mcp_commands_are_classified() {
        let config = Config {
            connections: vec![
                Connection {
                    kind: "mcp".into(),
                    command: "nonexistent_command_xyz".into(),
                },
                Connection {
                    kind: "mcp".into(),
                    command: "cargo".into(),
                },
            ],
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
            }],
        };

        let report = detect(&config);

        assert!(report.present.is_empty() && report.missing.is_empty());
    }
}
