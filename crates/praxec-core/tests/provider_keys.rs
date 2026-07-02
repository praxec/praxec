use praxec_core::provider_keys;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

// All tests share this lock so env-var mutations don't interleave.
fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[test]
fn resolve_path_honors_env_override() {
    let _guard = env_lock().lock().unwrap();
    // SAFETY: env_lock() serializes access across this test binary; no
    // other crate code touches PRAXEC_PROVIDER_KEYS_FILE.
    unsafe {
        std::env::set_var("PRAXEC_PROVIDER_KEYS_FILE", "/tmp/custom-provider-keys.env");
    }
    let p = provider_keys::resolve_path().expect("explicit override path resolves");
    assert_eq!(p, PathBuf::from("/tmp/custom-provider-keys.env"));
    unsafe {
        std::env::remove_var("PRAXEC_PROVIDER_KEYS_FILE");
    }
}

#[test]
fn resolve_path_defaults_under_praxec_home() {
    let _guard = env_lock().lock().unwrap();
    // SAFETY: env_lock() serializes access across this test binary; no
    // other crate code touches PRAXEC_PROVIDER_KEYS_FILE.
    unsafe {
        std::env::remove_var("PRAXEC_PROVIDER_KEYS_FILE");
    }
    let p = provider_keys::resolve_path().expect("home_dir resolves on supported platforms");
    // The default is dirs::home_dir().join(".praxec/providers.env"); on every
    // supported platform `dirs::home_dir` returns Some. Assert the suffix
    // rather than the absolute path (which varies by user).
    assert!(p.ends_with(".praxec/providers.env"), "got {}", p.display());
}

#[test]
fn resolve_path_whitespace_env_falls_through_to_default() {
    let _guard = env_lock().lock().unwrap();
    // SAFETY: env_lock() serializes access across this test binary.
    unsafe {
        std::env::set_var("PRAXEC_PROVIDER_KEYS_FILE", "   ");
    }
    let p = provider_keys::resolve_path().expect("whitespace env falls through to home_dir");
    assert!(
        p.ends_with(".praxec/providers.env") || p.ends_with("praxec-providers.env"),
        "whitespace env should fall through; got {}",
        p.display()
    );
    unsafe {
        std::env::remove_var("PRAXEC_PROVIDER_KEYS_FILE");
    }
}

#[test]
fn read_missing_file_returns_empty_map() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("nope.env");
    let m = provider_keys::read(&p).expect("missing-file is fine");
    assert!(m.is_empty());
}

#[test]
fn read_parses_two_var_file() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("providers.env");
    let mut f = std::fs::File::create(&p).unwrap();
    writeln!(f, "ANTHROPIC_API_KEY=sk-ant-aaa").unwrap();
    writeln!(f, "OPENAI_API_KEY=sk-bbb").unwrap();
    drop(f);
    #[cfg(unix)]
    {
        let mut perm = std::fs::metadata(&p).unwrap().permissions();
        perm.set_mode(0o600);
        std::fs::set_permissions(&p, perm).unwrap();
    }
    let m = provider_keys::read(&p).unwrap();
    let expected: BTreeMap<String, String> = BTreeMap::from([
        ("ANTHROPIC_API_KEY".into(), "sk-ant-aaa".into()),
        ("OPENAI_API_KEY".into(), "sk-bbb".into()),
    ]);
    assert_eq!(m, expected);
}

#[test]
fn read_skips_malformed_lines_and_blank_lines() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("providers.env");
    let mut f = std::fs::File::create(&p).unwrap();
    writeln!(f).unwrap();
    writeln!(f, "this-has-no-equals-sign").unwrap();
    writeln!(f, "ANTHROPIC_API_KEY=sk-ant-valid").unwrap();
    drop(f);
    #[cfg(unix)]
    {
        let mut perm = std::fs::metadata(&p).unwrap().permissions();
        perm.set_mode(0o600);
        std::fs::set_permissions(&p, perm).unwrap();
    }
    let m = provider_keys::read(&p).unwrap();
    assert_eq!(
        m.get("ANTHROPIC_API_KEY"),
        Some(&"sk-ant-valid".to_string())
    );
    assert_eq!(m.len(), 1);
}

#[test]
fn read_trims_whitespace_around_equals() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("providers.env");
    let mut f = std::fs::File::create(&p).unwrap();
    writeln!(f, "ANTHROPIC_API_KEY = sk-ant-aaa").unwrap();
    writeln!(f, "  OPENAI_API_KEY =sk-bbb  ").unwrap();
    drop(f);
    #[cfg(unix)]
    {
        let mut perm = std::fs::metadata(&p).unwrap().permissions();
        perm.set_mode(0o600);
        std::fs::set_permissions(&p, perm).unwrap();
    }
    let m = provider_keys::read(&p).unwrap();
    assert_eq!(m.get("ANTHROPIC_API_KEY"), Some(&"sk-ant-aaa".to_string()));
    assert_eq!(m.get("OPENAI_API_KEY"), Some(&"sk-bbb".to_string()));
}

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[test]
#[cfg(unix)]
fn write_atomic_creates_0600_file_in_0700_parent() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("sub").join("providers.env");
    let mut vars = BTreeMap::new();
    vars.insert("ANTHROPIC_API_KEY".into(), "sk-ant-aaa".into());
    provider_keys::write_atomic(&p, &vars).unwrap();

    let f_mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
    assert_eq!(f_mode, 0o600, "file mode should be 0600, got {:o}", f_mode);
    let parent_mode = std::fs::metadata(p.parent().unwrap())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        parent_mode, 0o700,
        "parent dir mode should be 0700, got {:o}",
        parent_mode
    );
}

#[test]
#[cfg(unix)]
fn read_rejects_world_readable_file() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("providers.env");
    std::fs::write(&p, "ANTHROPIC_API_KEY=x\n").unwrap();
    let mut perm = std::fs::metadata(&p).unwrap().permissions();
    perm.set_mode(0o644);
    std::fs::set_permissions(&p, perm).unwrap();

    let err = provider_keys::read(&p).unwrap_err();
    assert!(
        matches!(
            err,
            provider_keys::ProviderKeysError::PermissionsTooOpen { .. }
        ),
        "got {err:?}"
    );
}

#[test]
fn write_atomic_round_trips_via_read() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("providers.env");
    let mut vars = BTreeMap::new();
    vars.insert("ANTHROPIC_API_KEY".into(), "sk-ant-aaa".into());
    vars.insert("OPENAI_API_KEY".into(), "sk-bbb".into());
    provider_keys::write_atomic(&p, &vars).unwrap();
    let back = provider_keys::read(&p).unwrap();
    assert_eq!(back, vars);
}

use std::cell::RefCell;

#[test]
fn load_into_env_with_skips_already_set_vars() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("providers.env");
    let mut vars = BTreeMap::new();
    vars.insert("ANTHROPIC_API_KEY".into(), "file-value".into());
    vars.insert("OPENAI_API_KEY".into(), "file-openai".into());
    provider_keys::write_atomic(&p, &vars).unwrap();

    let written: RefCell<BTreeMap<String, String>> = RefCell::new(BTreeMap::new());
    let read_env = |k: &str| {
        if k == "ANTHROPIC_API_KEY" {
            Some("env-wins".to_string())
        } else {
            None
        }
    };
    let set_env = |k: &str, v: &str| {
        written.borrow_mut().insert(k.to_string(), v.to_string());
    };
    provider_keys::load_into_env_with(&p, read_env, set_env).unwrap();

    let w = written.borrow();
    assert_eq!(
        w.get("OPENAI_API_KEY"),
        Some(&"file-openai".to_string()),
        "file value loaded"
    );
    assert!(
        !w.contains_key("ANTHROPIC_API_KEY"),
        "env-set var must not be overwritten"
    );
}

#[test]
fn load_into_env_with_missing_file_is_silent_ok() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("nope.env");
    let written: RefCell<BTreeMap<String, String>> = RefCell::new(BTreeMap::new());
    let read_env = |_: &str| None;
    let set_env = |k: &str, v: &str| {
        written.borrow_mut().insert(k.to_string(), v.to_string());
    };
    provider_keys::load_into_env_with(&p, read_env, set_env).unwrap();
    assert!(written.borrow().is_empty());
}

#[test]
fn mask_value_shows_short_prefix_and_last4() {
    assert_eq!(
        provider_keys::mask_value("sk-ant-1234567890abcd"),
        "sk-ant-***abcd"
    );
}

#[test]
fn mask_value_handles_short_values() {
    // <=8 chars: don't risk leaking by showing prefix; mask entirely.
    assert_eq!(provider_keys::mask_value("short"), "***");
    assert_eq!(provider_keys::mask_value(""), "***");
}

#[test]
fn set_var_upserts_one_var_preserving_others() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("providers.env");
    let mut existing = BTreeMap::new();
    existing.insert("OPENAI_API_KEY".into(), "sk-keep".into());
    provider_keys::write_atomic(&p, &existing).unwrap();

    provider_keys::set_var(&p, "ANTHROPIC_API_KEY", "sk-new").unwrap();

    let back = provider_keys::read(&p).unwrap();
    assert_eq!(back.get("OPENAI_API_KEY"), Some(&"sk-keep".to_string()));
    assert_eq!(back.get("ANTHROPIC_API_KEY"), Some(&"sk-new".to_string()));
    assert_eq!(back.len(), 2);
}

#[test]
fn remove_provider_deletes_only_that_providers_vars() {
    use praxec_core::providers::ProviderId;
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("providers.env");
    let mut existing = BTreeMap::new();
    existing.insert("ANTHROPIC_API_KEY".into(), "sk-ant".into());
    existing.insert("OPENAI_API_KEY".into(), "sk-oai".into());
    existing.insert("AWS_ACCESS_KEY_ID".into(), "AKIA".into());
    existing.insert("AWS_SECRET_ACCESS_KEY".into(), "secret".into());
    existing.insert("AWS_REGION".into(), "us-east-1".into());
    provider_keys::write_atomic(&p, &existing).unwrap();

    provider_keys::remove_provider(&p, ProviderId::Bedrock).unwrap();

    let back = provider_keys::read(&p).unwrap();
    assert_eq!(back.get("ANTHROPIC_API_KEY"), Some(&"sk-ant".to_string()));
    assert_eq!(back.get("OPENAI_API_KEY"), Some(&"sk-oai".to_string()));
    assert!(!back.contains_key("AWS_ACCESS_KEY_ID"));
    assert!(!back.contains_key("AWS_SECRET_ACCESS_KEY"));
    assert!(!back.contains_key("AWS_REGION"));
}
