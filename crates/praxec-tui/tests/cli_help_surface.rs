use std::process::Command;

fn binary() -> String {
    env!("CARGO_BIN_EXE_px").to_string()
}

#[test]
fn completions_bash_emits_nonempty_script_with_command_name() {
    let out = Command::new(binary())
        .args(["completions", "bash"])
        .output()
        .expect("run completions bash");
    assert!(out.status.success(), "completions bash failed: {:?}", out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.is_empty(), "expected non-empty completion script");
    assert!(
        stdout.contains("px"),
        "expected completion script to reference the 'px' binary; got first 200 chars:\n{}",
        stdout.chars().take(200).collect::<String>()
    );
}

#[test]
fn completions_zsh_also_works() {
    let out = Command::new(binary())
        .args(["completions", "zsh"])
        .output()
        .expect("run completions zsh");
    assert!(out.status.success(), "completions zsh failed: {:?}", out);
    assert!(!out.stdout.is_empty());
}

#[test]
fn set_provider_keys_is_listed_in_help() {
    let out = Command::new(binary())
        .arg("--help")
        .output()
        .expect("run --help");
    assert!(out.status.success(), "--help failed: {:?}", out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("set-provider-keys"),
        "expected set-provider-keys in --help, got:\n{stdout}"
    );
}

#[test]
fn set_provider_keys_help_mentions_flags() {
    let out = Command::new(binary())
        .args(["set-provider-keys", "--help"])
        .output()
        .expect("run set-provider-keys --help");
    assert!(out.status.success(), "subcommand --help failed: {:?}", out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("--provider") && stdout.contains("--list"));
}

#[test]
fn set_provider_keys_path_prints_resolved_path() {
    let dir = tempfile::tempdir().unwrap();
    let want = dir.path().join("custom.env");
    let out = Command::new(binary())
        .env("PRAXEC_PROVIDER_KEYS_FILE", &want)
        .args(["set-provider-keys", "--path"])
        .output()
        .expect("run set-provider-keys --path");
    assert!(out.status.success(), "--path failed: {:?}", out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout.trim(), want.to_string_lossy());
}

#[test]
fn man_emits_roff_document() {
    let out = Command::new(binary()).arg("man").output().expect("run man");
    assert!(out.status.success(), "man failed: {:?}", out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.starts_with(".TH") || stdout.contains("\n.TH"),
        "expected roff `.TH` header; got first 200 chars:\n{}",
        stdout.chars().take(200).collect::<String>()
    );
}

#[test]
fn top_level_help_groups_subcommands_under_headings() {
    let out = Command::new(binary())
        .arg("--help")
        .output()
        .expect("--help");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    for heading in [
        "Run a governed agent",
        "Configure the harness",
        "Diagnostics & generators",
    ] {
        assert!(
            stdout.contains(heading),
            "expected heading '{heading}' in --help; got:\n{stdout}"
        );
    }
    for cmd in [
        "headless",
        "acp",
        "agent",
        "walk",
        "doctor",
        "mcp",
        "validate-models-config",
        "migrate-agents-from-cli",
        "set-provider-keys",
        "completions",
        "man",
    ] {
        assert!(
            stdout.contains(cmd),
            "expected '{cmd}' in --help; got:\n{stdout}"
        );
    }
}

#[test]
fn walk_long_help_mentions_deterministic_interpreter() {
    let out = Command::new(binary())
        .args(["help", "walk"])
        .output()
        .expect("help walk");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("deterministic interpreter"),
        "expected `help walk` to surface long_about; got:\n{stdout}"
    );
}

#[test]
fn doctor_long_help_mentions_preflight() {
    let out = Command::new(binary())
        .args(["help", "doctor"])
        .output()
        .expect("help doctor");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lower = stdout.to_lowercase();
    assert!(lower.contains("pre-flight") || lower.contains("preflight"));
}

#[test]
fn set_provider_keys_long_help_mentions_file_location() {
    let out = Command::new(binary())
        .args(["help", "set-provider-keys"])
        .output()
        .expect("help set-provider-keys");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("providers.env"));
}
