//! FMECA U2 + T1 tests: eager primary-binding auth verification + the
//! CLI-flag/YAML mutual-exclusion check (the latter lives in main.rs
//! but is exercised via the public preflight surface).

use praxec_core::model_resolver::preflight::{
    api_key_env_for, classify_outcome, probe_binding, PreflightOutcome,
};
use praxec_core::model_resolver::provider_probe::probe_client;
use praxec_core::model_resolver::{
    verify_primary_bindings, Binding, ConfigSource, FailureClass, ModelRef, ModelsFile,
    PreflightError, Provider, ProviderFeatures, Resolver,
};
use praxec_core::providers::ProviderId;
use std::path::PathBuf;
use std::sync::OnceLock;

use tokio::sync::Mutex;

// All tests in this file manipulate env vars; serialise to prevent
// interleaving. Uses tokio's async-aware Mutex so the guard can be
// held across `.await` without tripping the `await_holding_lock`
// clippy lint (`std::sync::Mutex` triggers it; this is the documented
// pattern for serialising env-touching async tests).
fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn clear_env() {
    for var in [
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "GEMINI_API_KEY",
        "PRAXEC_SKIP_PREFLIGHT",
        "ANTHROPIC_BASE_URL",
        "OPENAI_BASE_URL",
    ] {
        std::env::remove_var(var);
    }
}

fn resolver_from(yaml: &str) -> Resolver {
    let file = ModelsFile::from_yaml(yaml).expect("yaml parses");
    Resolver::from_loaded(
        file,
        ConfigSource::Project(PathBuf::from("/tmp/models.yaml")),
    )
}

// ── api_key_env_for ─────────────────────────────────────────────────────────

#[test]
fn api_key_env_per_provider() {
    assert_eq!(
        api_key_env_for(&Provider::Known(ProviderId::Anthropic)),
        Some("ANTHROPIC_API_KEY")
    );
    assert_eq!(
        api_key_env_for(&Provider::Known(ProviderId::Openai)),
        Some("OPENAI_API_KEY")
    );
    assert_eq!(
        api_key_env_for(&Provider::Known(ProviderId::Gemini)),
        Some("GEMINI_API_KEY")
    );
    assert_eq!(api_key_env_for(&Provider::Known(ProviderId::Ollama)), None);
    assert_eq!(
        api_key_env_for(&Provider::Known(ProviderId::Llamacpp)),
        None
    );
    assert_eq!(
        api_key_env_for(&Provider::Custom {
            endpoint: "https://x".into(),
        }),
        None
    );
}

// ── probe_binding ───────────────────────────────────────────────────────────

#[tokio::test]
async fn probe_present_key_but_unreachable_endpoint_warns_not_fails() {
    // FMECA U2: a present credential whose provider endpoint is
    // unreachable must NOT block startup — it's a transient `Warn`, and
    // the runtime CoR routes around it. We point the probe at a
    // non-routable base URL (RFC 5737 TEST-NET-1, no listener) via the
    // `*_BASE_URL` override so the probe fails at the transport layer
    // without depending on a real provider. This pins "only 401/403 / missing
    // credential block startup", which is the whole point of the auth probe.
    let _g = env_lock().lock().await;
    clear_env();
    std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-test");
    // 192.0.2.0/24 is reserved for documentation/tests and is not routed.
    std::env::set_var("ANTHROPIC_BASE_URL", "http://192.0.2.1:1");
    let b = Binding {
        provider: Provider::Known(ProviderId::Anthropic),
        model: "claude-sonnet-4-6".into(),
        features: ProviderFeatures::None,
    };
    let client = probe_client();
    let outcome = probe_binding(&client, &b).await;
    assert!(
        matches!(outcome, PreflightOutcome::Warn { .. }),
        "present key + unreachable endpoint must Warn (non-blocking), not Fail: {outcome:?}"
    );
    // And it must NOT be a startup-blocking error.
    let binding_for_classify = b.clone();
    assert!(
        classify_outcome("coding", &binding_for_classify, outcome).is_none(),
        "transient unreachable must not block startup"
    );
}

#[tokio::test]
async fn probe_without_credential_reports_missing() {
    let _g = env_lock().lock().await;
    clear_env();
    let b = Binding {
        provider: Provider::Known(ProviderId::Openai),
        model: "gpt-5".into(),
        features: ProviderFeatures::None,
    };
    let client = probe_client();
    let outcome = probe_binding(&client, &b).await;
    assert!(
        matches!(
            outcome,
            PreflightOutcome::MissingCredential {
                env_var: "OPENAI_API_KEY"
            }
        ),
        "got {outcome:?}"
    );
}

#[tokio::test]
async fn probe_ollama_no_credential_required_returns_ok() {
    let _g = env_lock().lock().await;
    clear_env();
    let b = Binding {
        provider: Provider::Known(ProviderId::Ollama),
        model: "llama3".into(),
        features: ProviderFeatures::None,
    };
    let client = probe_client();
    assert!(matches!(
        probe_binding(&client, &b).await,
        PreflightOutcome::Ok
    ));
}

// ── verify_primary_bindings ─────────────────────────────────────────────────

#[tokio::test]
async fn primary_missing_credential_fails_startup() {
    let _g = env_lock().lock().await;
    clear_env();
    let yaml = r#"
version: 1
default:
  - provider: { name: anthropic }
    model: claude-sonnet-4-6
"#;
    let r = resolver_from(yaml);
    let d = ModelRef::parse("coding").unwrap();
    let err = verify_primary_bindings(&r, &[d])
        .await
        .expect_err("missing ANTHROPIC_API_KEY = startup error");
    assert!(matches!(
        err.first(),
        Some(PreflightError::MissingCredential { .. })
    ));
}

#[tokio::test]
async fn primary_with_credential_passes_startup_when_probe_is_transient() {
    // A present credential where the auth probe can't reach the provider
    // (unreachable endpoint → transient Warn) must NOT block startup.
    // Only a 401/403 or a missing credential blocks. We use the
    // `ANTHROPIC_BASE_URL` override to point at a non-routable host so the
    // test is hermetic (no live provider call, no real key needed).
    let _g = env_lock().lock().await;
    clear_env();
    std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-test");
    std::env::set_var("ANTHROPIC_BASE_URL", "http://192.0.2.1:1");
    let yaml = r#"
version: 1
default:
  - provider: { name: anthropic }
    model: claude-sonnet-4-6
"#;
    let r = resolver_from(yaml);
    let d = ModelRef::parse("coding").unwrap();
    verify_primary_bindings(&r, &[d])
        .await
        .expect("present credential + transient probe = passes startup");
}

#[tokio::test]
async fn skip_env_bypasses_preflight() {
    let _g = env_lock().lock().await;
    clear_env();
    // No credentials set, but SKIP env is on → preflight must pass.
    std::env::set_var("PRAXEC_SKIP_PREFLIGHT", "1");
    let yaml = r#"
version: 1
default:
  - provider: { name: anthropic }
    model: claude-sonnet-4-6
"#;
    let r = resolver_from(yaml);
    let d = ModelRef::parse("coding").unwrap();
    verify_primary_bindings(&r, &[d])
        .await
        .expect("SKIP=1 bypasses preflight even with missing creds");
}

// ── classify_outcome (dispatch logic) ──────────────────────────────────────

#[test]
fn primary_rate_limit_logs_warning_but_passes() {
    // FMECA U2: 429 (and 404 / network) on a primary preflight is a
    // transient class — the resolver's runtime CoR will route around it.
    // Startup must NOT fail. classify_outcome is the pure dispatch helper
    // both verify_* functions delegate to; testing it pins the contract
    // without needing real HTTP plumbing.
    let binding = Binding {
        provider: Provider::Known(ProviderId::Anthropic),
        model: "claude-sonnet-4-6".into(),
        features: ProviderFeatures::None,
    };
    let outcome = PreflightOutcome::Warn {
        class: FailureClass::RateLimit429,
        detail: "429 from anthropic".into(),
    };
    assert!(
        classify_outcome("coding", &binding, outcome).is_none(),
        "429 on primary must warn-but-pass; only 401/403/missing-cred block startup"
    );
}

#[test]
fn primary_auth_401_blocks_startup() {
    // Inverse of the above — pins the boundary: 401 IS a hard failure.
    let binding = Binding {
        provider: Provider::Known(ProviderId::Anthropic),
        model: "claude-sonnet-4-6".into(),
        features: ProviderFeatures::None,
    };
    let outcome = PreflightOutcome::Fail {
        class: FailureClass::Auth401,
        detail: "401 unauthorized".into(),
    };
    let err = classify_outcome("coding", &binding, outcome)
        .expect("401 on primary must produce a startup error");
    assert!(matches!(err, PreflightError::PrimaryAuthFailed { .. }));
}

#[tokio::test]
async fn preflight_dedupes_same_primary_across_delegates() {
    // Multiple delegates that resolve to the same primary binding
    // should only probe once. We can't observe the probe count
    // directly without an injectable HTTP client, but we CAN observe
    // that the error list isn't repeated.
    let _g = env_lock().lock().await;
    clear_env();
    let yaml = r#"
version: 1
default:
  - provider: { name: anthropic }
    model: claude-sonnet-4-6
"#;
    let r = resolver_from(yaml);
    let delegates = vec![
        ModelRef::parse("coding").unwrap(),
        ModelRef::parse("reasoning").unwrap(),
        ModelRef::parse("prose").unwrap(),
    ];
    let err = verify_primary_bindings(&r, &delegates)
        .await
        .expect_err("missing cred");
    assert_eq!(
        err.len(),
        1,
        "the SAME primary binding's missing credential must surface ONCE, not per-delegate"
    );
}
