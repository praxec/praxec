//! Named-account registry loaded from `accounts.yaml`.
//!
//! # Schema
//!
//! ```yaml
//! accounts:
//!   - provider: anthropic
//!     name: work
//!   - provider: openai
//!     name: personal
//!   - provider: openai
//!     name: work
//! ```
//!
//! Secrets are **never** stored here; each entry only names an account.
//! The corresponding env var is resolved by
//! [`crate::providers::Credentials::account_env_var`] at load time —
//! `<PRIMARY_KEY_VAR>_<NAME_UPPER>` (e.g. `OPENAI_API_KEY_WORK`).
//!
//! # Load-time fail-fast rules
//!
//! An entry is rejected (returns [`AccountError`]) if:
//! 1. The provider slug is unknown.
//! 2. The provider is keyless/native (ollama, llamacpp) — named accounts are
//!    meaningless without credentials.
//! 3. The provider has no registry (its env key is absent and it is not
//!    keyless) **and** the entry's key env var is also absent.
//!
//! These map to decision-2 in the spec.

use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

use serde::Deserialize;
use thiserror::Error;

use crate::providers::{Credentials, ProviderId};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A single validated account entry.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Account {
    pub provider: String,
    pub name: String,
}

/// Errors produced during YAML loading / validation (all fail-fast).
#[derive(Debug, Error)]
pub enum AccountError {
    #[error("unknown provider slug {slug:?} in accounts.yaml")]
    UnknownProvider { slug: String },

    #[error(
        "provider {provider:?} is keyless/native — named accounts are not supported \
         (account {name:?} rejected)"
    )]
    KeylessProvider { provider: String, name: String },

    #[error(
        "provider {provider:?} has no registry: primary env var is unset and account \
         key env var {var:?} is also absent (account {name:?} rejected)"
    )]
    NoRegistry {
        provider: String,
        name: String,
        var: String,
    },

    #[error("failed to read accounts.yaml at {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },

    #[error("failed to parse accounts.yaml at {path}: {source}")]
    Parse {
        path: String,
        source: serde_yaml::Error,
    },
}

/// The loaded, validated account registry.
#[derive(Debug, Default, Clone)]
pub struct AccountRegistry {
    /// `(provider_slug, account_name)` → env var holding the secret.
    inner: HashMap<(String, String), String>,
}

impl AccountRegistry {
    /// Look up whether a named account is registered **and** its key env var
    /// is currently set in the environment.  This is the generalised form of
    /// [`crate::providers::vendor_available`] for named accounts.
    pub fn account_available(&self, provider: &str, account: &str) -> bool {
        let key = (provider.to_string(), account.to_string());
        match self.inner.get(&key) {
            Some(var) => std::env::var(var).is_ok(),
            None => false,
        }
    }

    /// All registered `(provider, account)` pairs.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &str)> {
        self.inner.keys().map(|(p, a)| (p.as_str(), a.as_str()))
    }

    /// The set of account names registered for `provider`.
    pub fn accounts_for_provider<'a>(&'a self, provider: &str) -> HashSet<&'a str> {
        self.inner
            .keys()
            .filter_map(|(p, a)| {
                if p == provider {
                    Some(a.as_str())
                } else {
                    None
                }
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// YAML schema (private)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawFile {
    accounts: Vec<RawEntry>,
}

#[derive(Debug, Deserialize)]
struct RawEntry {
    provider: String,
    name: String,
}

// ---------------------------------------------------------------------------
// Loader
// ---------------------------------------------------------------------------

/// Parse and validate `accounts.yaml` at `path`.
///
/// Returns a fully validated [`AccountRegistry`] or a **fail-fast**
/// [`AccountError`] describing the first rejected entry.
pub fn load_accounts(path: &Path) -> Result<AccountRegistry, AccountError> {
    let raw = std::fs::read_to_string(path).map_err(|e| AccountError::Io {
        path: path.display().to_string(),
        source: e,
    })?;

    parse_accounts_yaml(&raw, path.display().to_string().as_str())
}

/// Parse and validate `accounts.yaml` from a YAML string.
///
/// `source_label` is used only in error messages.
pub fn parse_accounts_yaml(
    yaml: &str,
    source_label: &str,
) -> Result<AccountRegistry, AccountError> {
    let file: RawFile = serde_yaml::from_str(yaml).map_err(|e| AccountError::Parse {
        path: source_label.to_string(),
        source: e,
    })?;

    let mut inner: HashMap<(String, String), String> = HashMap::with_capacity(file.accounts.len());

    for entry in file.accounts {
        let pid = ProviderId::from_slug(&entry.provider).ok_or_else(|| {
            AccountError::UnknownProvider {
                slug: entry.provider.clone(),
            }
        })?;

        // Rule 1 — keyless providers reject named accounts.
        if pid.credentials() == Credentials::None {
            return Err(AccountError::KeylessProvider {
                provider: entry.provider.clone(),
                name: entry.name.clone(),
            });
        }

        // Rule 2 — the derived account key env var must be resolvable.
        let var = pid
            .credentials()
            .account_env_var(&entry.name)
            .expect("credentials are not None; account_env_var is Some");

        // Fail-fast if neither the primary env var nor the account-specific
        // one is set — the provider has "no registry".
        let primary_present = pid
            .credentials()
            .primary()
            .map(|v| std::env::var(v).is_ok())
            .unwrap_or(false);
        let account_present = std::env::var(&var).is_ok();

        if !primary_present && !account_present {
            return Err(AccountError::NoRegistry {
                provider: entry.provider.clone(),
                name: entry.name.clone(),
                var,
            });
        }

        inner.insert((entry.provider, entry.name), var);
    }

    Ok(AccountRegistry { inner })
}

// ---------------------------------------------------------------------------
// Standalone convenience — mirrors `providers::account_available` but uses
// the loaded registry (i.e. validates the account is declared, not just
// that the env var happens to exist).
// ---------------------------------------------------------------------------

/// True if `provider`/`account` is declared in `registry` and its key env var
/// is currently set in the environment.
///
/// Prefer this over the bare [`crate::providers::account_available`] wherever
/// a registry is available, because this also validates the account was
/// explicitly declared in `accounts.yaml`.
pub fn account_available(registry: &AccountRegistry, provider: &str, account: &str) -> bool {
    registry.account_available(provider, account)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Temporarily set env vars for the duration of `f`, then restore.
    ///
    /// NOTE: env mutation is process-global.  These tests must not run
    /// concurrently with other tests that read the same env vars; they are
    /// serialised by Rust's test harness (single threaded by default).
    fn with_env<F: FnOnce()>(vars: &[(&str, &str)], f: F) {
        for (k, v) in vars {
            // SAFETY (edition 2024): tests are serialised by the single-threaded
            // harness; no other thread reads these vars concurrently.
            unsafe { std::env::set_var(k, v) };
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        for (k, _) in vars {
            unsafe { std::env::remove_var(k) };
        }
        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }

    // -----------------------------------------------------------------------
    // Happy-path
    // -----------------------------------------------------------------------

    #[test]
    fn single_account_roundtrip() {
        with_env(&[("ANTHROPIC_API_KEY_WORK", "sk-test")], || {
            let yaml = "accounts:\n  - provider: anthropic\n    name: work\n";
            let reg = parse_accounts_yaml(yaml, "test").expect("should parse");
            assert!(reg.account_available("anthropic", "work"));
            assert!(!reg.account_available("anthropic", "personal"));
        });
    }

    #[test]
    fn multiple_accounts_same_provider() {
        with_env(
            &[
                ("OPENAI_API_KEY_WORK", "sk-w"),
                ("OPENAI_API_KEY_PERSONAL", "sk-p"),
            ],
            || {
                let yaml = "accounts:\n\
                            \x20 - provider: openai\n\
                            \x20   name: work\n\
                            \x20 - provider: openai\n\
                            \x20   name: personal\n";
                let reg = parse_accounts_yaml(yaml, "test").expect("should parse");
                assert!(reg.account_available("openai", "work"));
                assert!(reg.account_available("openai", "personal"));
                let accounts = reg.accounts_for_provider("openai");
                assert_eq!(accounts.len(), 2);
                assert!(accounts.contains("work"));
                assert!(accounts.contains("personal"));
            },
        );
    }

    #[test]
    fn hyphen_in_account_name_normalised() {
        with_env(&[("OPENAI_API_KEY_MY_ORG", "sk-x")], || {
            let yaml = "accounts:\n  - provider: openai\n    name: my-org\n";
            let reg = parse_accounts_yaml(yaml, "test").expect("should parse");
            assert!(reg.account_available("openai", "my-org"));
        });
    }

    // -----------------------------------------------------------------------
    // Fail-fast: unknown provider
    // -----------------------------------------------------------------------

    #[test]
    fn unknown_provider_is_rejected() {
        let yaml = "accounts:\n  - provider: mystery-ai\n    name: work\n";
        let err = parse_accounts_yaml(yaml, "test").unwrap_err();
        assert!(
            matches!(err, AccountError::UnknownProvider { ref slug } if slug == "mystery-ai"),
            "wrong error: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // Fail-fast: keyless provider
    // -----------------------------------------------------------------------

    #[test]
    fn keyless_provider_rejects_named_account() {
        for slug in ["ollama", "llamacpp"] {
            let yaml = format!("accounts:\n  - provider: {slug}\n    name: local\n");
            let err = parse_accounts_yaml(&yaml, "test").unwrap_err();
            assert!(
                matches!(err, AccountError::KeylessProvider { .. }),
                "expected KeylessProvider for {slug}, got: {err}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Fail-fast: no registry (key absent)
    // -----------------------------------------------------------------------

    #[test]
    fn missing_key_env_var_is_rejected() {
        // Make absolutely sure neither var is set (edition-2024 unsafe env).
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
            std::env::remove_var("ANTHROPIC_API_KEY_NOKEY");
        }

        let yaml = "accounts:\n  - provider: anthropic\n    name: nokey\n";
        let err = parse_accounts_yaml(yaml, "test").unwrap_err();
        assert!(
            matches!(
                err,
                AccountError::NoRegistry {
                    ref provider,
                    ref name,
                    ..
                } if provider == "anthropic" && name == "nokey"
            ),
            "wrong error: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // account_available convenience wrapper
    // -----------------------------------------------------------------------

    #[test]
    fn convenience_account_available() {
        with_env(&[("GEMINI_API_KEY_LABS", "key")], || {
            let yaml = "accounts:\n  - provider: gemini\n    name: labs\n";
            let reg = parse_accounts_yaml(yaml, "test").expect("should parse");
            assert!(account_available(&reg, "gemini", "labs"));
            assert!(!account_available(&reg, "gemini", "other"));
        });
    }

    // -----------------------------------------------------------------------
    // entries iterator
    // -----------------------------------------------------------------------

    #[test]
    fn entries_reports_all_accounts() {
        with_env(
            &[
                ("OPENROUTER_API_KEY_A", "k1"),
                ("OPENROUTER_API_KEY_B", "k2"),
            ],
            || {
                let yaml = "accounts:\n\
                            \x20 - provider: openrouter\n\
                            \x20   name: a\n\
                            \x20 - provider: openrouter\n\
                            \x20   name: b\n";
                let reg = parse_accounts_yaml(yaml, "test").expect("should parse");
                let entries: Vec<_> = reg.entries().collect();
                assert_eq!(entries.len(), 2);
            },
        );
    }
}
