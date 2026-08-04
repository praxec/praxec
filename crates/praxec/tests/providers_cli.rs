//! `praxec providers` — native provider-key management, at the CLI surface.
//!
//! Proves key management works with ONLY the gateway binary (no `px`, no shell
//! script). All tests pin `$PRAXEC_PROVIDER_KEYS_FILE` to a temp file so they
//! never touch the operator's real `~/.config/praxec/providers.env`, and use
//! `--key-stdin` so no TTY prompt is involved.

use std::io::Write;
use std::process::{Command, Output, Stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_praxec")
}

/// Run `praxec providers <args>` with the keys file isolated to `keys_file`,
/// optionally feeding `stdin`.
fn run(keys_file: &str, args: &[&str], stdin: Option<&str>) -> Output {
    let mut child = Command::new(bin())
        .arg("providers")
        .args(args)
        .env("PRAXEC_PROVIDER_KEYS_FILE", keys_file)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn praxec");
    if let Some(s) = stdin {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(s.as_bytes())
            .expect("write stdin");
    }
    child.wait_with_output().expect("wait")
}

#[test]
fn list_on_empty_reports_none_configured() {
    let td = tempfile::tempdir().unwrap();
    let kf = td.path().join("providers.env");
    let out = run(kf.to_str().unwrap(), &["list"], None);
    let so = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{so}");
    assert!(so.contains("no provider keys configured"), "{so}");
}

#[test]
fn set_via_stdin_then_list_shows_it_masked() {
    let td = tempfile::tempdir().unwrap();
    let kf = td.path().join("providers.env");
    let kfs = kf.to_str().unwrap();

    let set = run(
        kfs,
        &["set", "--provider", "openrouter", "--key-stdin"],
        Some("sk-secret-value-123\n"),
    );
    assert!(
        set.status.success(),
        "set must succeed:\n{}",
        String::from_utf8_lossy(&set.stderr)
    );
    // The key is written to the isolated file.
    let contents = std::fs::read_to_string(&kf).unwrap();
    assert!(
        contents.contains("OPENROUTER_API_KEY="),
        "the key is written: {contents}"
    );

    // `list` shows the var but MASKS the secret — never the raw value.
    let list = run(kfs, &["list"], None);
    let so = String::from_utf8_lossy(&list.stdout);
    assert!(so.contains("OPENROUTER_API_KEY="), "{so}");
    assert!(
        !so.contains("sk-secret-value-123"),
        "list must MASK the secret, never print it raw:\n{so}"
    );
}

#[test]
fn path_prints_the_resolved_keys_file() {
    let td = tempfile::tempdir().unwrap();
    let kf = td.path().join("providers.env");
    let out = run(kf.to_str().unwrap(), &["path"], None);
    let so = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{so}");
    assert!(
        so.trim().ends_with("providers.env"),
        "prints the resolved keys-file path: {so}"
    );
}

#[test]
fn unknown_provider_fails_with_the_valid_set() {
    let td = tempfile::tempdir().unwrap();
    let kf = td.path().join("providers.env");
    let out = run(
        kf.to_str().unwrap(),
        &["set", "--provider", "nonsense", "--key-stdin"],
        None,
    );
    assert!(!out.status.success(), "unknown provider must fail");
    let se = String::from_utf8_lossy(&out.stderr);
    assert!(
        se.contains("unknown provider 'nonsense'"),
        "names the bad slug + lists valid ones:\n{se}"
    );
}

#[test]
fn set_from_env_reads_the_matching_var() {
    let td = tempfile::tempdir().unwrap();
    let kf = td.path().join("providers.env");
    let kfs = kf.to_str().unwrap();
    // --from-env reads OPENROUTER_API_KEY from the environment.
    let out = Command::new(bin())
        .args(["providers", "set", "--provider", "openrouter", "--from-env"])
        .env("PRAXEC_PROVIDER_KEYS_FILE", kfs)
        .env("OPENROUTER_API_KEY", "sk-from-the-env")
        .output()
        .expect("run");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let contents = std::fs::read_to_string(&kf).unwrap();
    assert!(
        contents.contains("OPENROUTER_API_KEY=sk-from-the-env"),
        "the env value is written: {contents}"
    );
}
