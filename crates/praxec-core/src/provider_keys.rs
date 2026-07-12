//! Provider API key file backend.
//!
//! Writes a flat dotenv file at `~/.config/praxec/providers.env` (the XDG
//! config dir, alongside the gateway config; legacy `~/.praxec/providers.env`
//! is still read as a fallback — see [`resolve_path`]), override via
//! `$PRAXEC_PROVIDER_KEYS_FILE`, with mode 0600 inside a 0700 parent dir.
//! Loaded into env at startup, existing env vars taking precedence (CI
//! overrides file).
//!
//! File-backed (not OS keyring) so agent sub-processes spawned by
//! `walk` / `headless` can read the keys without UI prompts and so
//! the path works identically across macOS, Linux, and WSL2.
//!
//! This is the **pure backend** (path resolution + file I/O); the
//! `set-provider-keys` clap CLI that drives it lives in
//! `praxec-tui`, and the Mission Control setup gate
//! (`praxec-cockpit`) reads/writes through it too.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::providers::ProviderId;

/// Resolve the on-disk path for the provider-keys file. Precedence:
/// 1. `$PRAXEC_PROVIDER_KEYS_FILE` if set + non-empty.
/// 2. An EXISTING `~/.config/praxec/providers.env` (the XDG config dir, beside
///    the gateway config) — checked first.
/// 3. An EXISTING `~/.praxec/providers.env` (legacy home dot-dir).
/// 4. Otherwise `~/.config/praxec/providers.env` (so a fresh `set-provider-keys`
///    writes beside the config), falling back to `~/.praxec/providers.env`.
///
/// The XDG-config-first order fixes the v0.0.17 mismatch (dogfood Finding 17):
/// keys written next to the gateway config in `~/.config/praxec/` were never
/// auto-loaded because only `~/.praxec/` was consulted → no credentials → agent
/// model calls failed fast (surfacing as a spurious sub-second "timeout").
///
/// When neither a config nor home dir can be resolved (some sandboxed CI), this
/// returns [`ProviderKeysError::NoConfigDir`] rather than a relative CWD path —
/// writing secrets to a world-adjacent relative location is a real hazard; set
/// `$PRAXEC_PROVIDER_KEYS_FILE` to an explicit absolute path instead.
pub fn resolve_path() -> Result<PathBuf, ProviderKeysError> {
    if let Ok(p) = std::env::var("PRAXEC_PROVIDER_KEYS_FILE") {
        if !p.trim().is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    let xdg = dirs::config_dir().map(|d| d.join("praxec").join("providers.env"));
    let home = dirs::home_dir().map(|d| d.join(".praxec").join("providers.env"));
    // Prefer an existing file — the XDG config dir (where the gateway config
    // lives) first, then the legacy home dot-dir.
    if let Some(p) = xdg.as_ref().filter(|p| p.exists()) {
        return Ok(p.clone());
    }
    if let Some(p) = home.as_ref().filter(|p| p.exists()) {
        return Ok(p.clone());
    }
    // Neither exists yet: default to the XDG config path, else the legacy home path.
    xdg.or(home).ok_or(ProviderKeysError::NoConfigDir)
}

/// Errors from the provider-keys file backend.
#[derive(Debug, thiserror::Error)]
pub enum ProviderKeysError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "file {path} has permissions {mode:o}; expected 0600. \
         Fix with: chmod 600 {path}"
    )]
    PermissionsTooOpen { path: String, mode: u32 },
    #[error(
        "cannot locate a config directory for the provider-keys file. \
         Set $PRAXEC_PROVIDER_KEYS_FILE to an explicit absolute path."
    )]
    NoConfigDir,
}

/// Read the provider-keys file. Returns an empty map if the file does
/// not exist. Malformed lines (no `=`) are skipped with a warn log;
/// blank lines are ignored. Values are taken verbatim (no quote
/// stripping) — the writer doesn't quote, so the reader doesn't unquote.
/// Surrounding whitespace on both keys and values is trimmed so
/// hand-edited files with `KEY = value` syntax round-trip correctly.
pub fn read(path: &Path) -> Result<BTreeMap<String, String>, ProviderKeysError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(ProviderKeysError::Io(e)),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(ProviderKeysError::PermissionsTooOpen {
                path: path.display().to_string(),
                mode,
            });
        }
    }

    let mut out = BTreeMap::new();
    for (i, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match line.split_once('=') {
            Some((k, v)) => {
                out.insert(k.trim().to_string(), v.trim().to_string());
            }
            None => {
                tracing::warn!(
                    file = %path.display(),
                    line_no = i + 1,
                    "skipping malformed line in provider-keys file"
                );
            }
        }
    }
    Ok(out)
}

/// Write the provider-keys map atomically: tempfile in the same dir,
/// chmod 0600, then rename over the target. Parent dir created with
/// mode 0700 if missing. Atomic rename means a partial-write torn
/// state is impossible.
pub fn write_atomic(path: &Path, vars: &BTreeMap<String, String>) -> Result<(), ProviderKeysError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = std::fs::metadata(parent)?.permissions();
            perm.set_mode(0o700);
            std::fs::set_permissions(parent, perm)?;
        }
    }

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temp = tempfile::Builder::new()
        .prefix(".providers.env.")
        .suffix(".tmp")
        .tempfile_in(parent)?;

    {
        use std::io::Write;
        let mut f = temp.as_file();
        for (k, v) in vars {
            writeln!(f, "{k}={v}")?;
        }
        f.flush()?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(temp.path())?.permissions();
        perm.set_mode(0o600);
        std::fs::set_permissions(temp.path(), perm)?;
    }

    temp.persist(path)
        .map_err(|e| ProviderKeysError::Io(e.error))?;
    Ok(())
}

/// Inject-friendly load. Read the file, then for each `(k, v)`:
/// - if `read_env(k)` returns Some, leave it (env wins over file).
/// - otherwise call `set_env(k, v)`.
///
/// Errors are returned, not silently swallowed — the production
/// wrapper in `praxec-tui` decides the swallow policy.
pub fn load_into_env_with(
    path: &Path,
    read_env: impl Fn(&str) -> Option<String>,
    mut set_env: impl FnMut(&str, &str),
) -> Result<(), ProviderKeysError> {
    let vars = read(path)?;
    for (k, v) in vars {
        if read_env(&k).is_some() {
            continue;
        }
        set_env(&k, &v);
    }
    Ok(())
}

/// Production wrapper. Loads the resolved provider-keys file into the process
/// env (env vars already set win over the file). A missing file is a silent OK;
/// a path-resolution or read error logs a single warning and continues.
///
/// MUST be called synchronously at the top of `main()`, before the first
/// `.await`, so no spawned task can race on the process env. Both the `px`
/// (praxec-tui) and `praxec` (gateway) binaries call this — the gateway used to
/// skip it, so `serve` never picked up `~/.praxec/providers.env` and every
/// `kind: agent` / `kind: llm` step failed for want of a key.
pub fn load_into_env_if_present() {
    let path = match resolve_path() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "cannot locate provider-keys file; skipping load");
            return;
        }
    };
    let result = load_into_env_with(
        &path,
        |k| std::env::var(k).ok(),
        // SAFETY: called synchronously at the top of `main()` before the first
        // `.await`, so no `tokio::spawn`-ed task exists yet that could race on
        // the process env.
        |k, v| unsafe { std::env::set_var(k, v) },
    );
    if let Err(e) = result {
        tracing::warn!(
            error = %e,
            path = %path.display(),
            "failed to load provider-keys file"
        );
    }
}

/// Mask a secret value for `--list` display. Long values become
/// `<7-char-prefix>***<last-4>`; values of 8 chars or less are masked
/// entirely (the prefix-plus-last4 form would leak too much of the
/// original).
pub fn mask_value(s: &str) -> String {
    if s.len() <= 8 {
        return "***".to_string();
    }
    let prefix: String = s.chars().take(7).collect();
    let last4: String = s
        .chars()
        .rev()
        .take(4)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{prefix}***{last4}")
}

/// Upsert a single env var in the file. Reads, mutates one key, writes
/// atomically. Use this for the CLI `set` path and the cockpit setup
/// gate; the interactive walker composes multiple `set_var` calls.
pub fn set_var(path: &Path, key: &str, value: &str) -> Result<(), ProviderKeysError> {
    let mut vars = read(path)?;
    vars.insert(key.to_string(), value.to_string());
    write_atomic(path, &vars)
}

/// Delete every env var that belongs to the given provider.
pub fn remove_provider(path: &Path, provider: ProviderId) -> Result<(), ProviderKeysError> {
    let mut vars = read(path)?;
    for k in provider.credentials().env_vars() {
        vars.remove(k);
    }
    write_atomic(path, &vars)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ProviderId;

    /// resolve_path honors an explicit `$PRAXEC_PROVIDER_KEYS_FILE` override
    /// and returns it as `Ok` — the fallible signature must not break the
    /// primary (operator-set) path. Uses a process-global env var, so this test
    /// is the sole writer of that var in this module.
    #[test]
    fn resolve_path_returns_explicit_override() {
        // SAFETY: single-threaded test setup; this is the only test touching
        // this env var, and we restore it immediately after reading.
        let prior = std::env::var("PRAXEC_PROVIDER_KEYS_FILE").ok();
        unsafe { std::env::set_var("PRAXEC_PROVIDER_KEYS_FILE", "/tmp/explicit-keys.env") };
        let resolved = resolve_path().expect("explicit override must resolve");
        assert_eq!(resolved, PathBuf::from("/tmp/explicit-keys.env"));
        unsafe {
            match prior {
                Some(v) => std::env::set_var("PRAXEC_PROVIDER_KEYS_FILE", v),
                None => std::env::remove_var("PRAXEC_PROVIDER_KEYS_FILE"),
            }
        }
    }

    /// CMP-005 regression: the `gemini` CLI alias must write GEMINI_API_KEY —
    /// the same var that core's preflight, the live probe, and aether-llm's
    /// gemini provider all read. Asserts against the core catalog directly.
    #[test]
    fn gemini_env_var_matches_core() {
        assert_eq!(
            ProviderId::Gemini.credentials().primary(),
            Some("GEMINI_API_KEY")
        );
    }

    /// Single-key providers must carry the expected env var in the core
    /// catalog — no independent re-encoding (CMP-026).
    #[test]
    fn single_key_providers_have_expected_vars() {
        assert_eq!(
            ProviderId::Anthropic.credentials().primary(),
            Some("ANTHROPIC_API_KEY")
        );
        assert_eq!(
            ProviderId::Openai.credentials().primary(),
            Some("OPENAI_API_KEY")
        );
        assert_eq!(
            ProviderId::Gemini.credentials().primary(),
            Some("GEMINI_API_KEY")
        );
    }

    #[test]
    fn openrouter_is_a_valid_provider_slug() {
        assert_eq!(
            ProviderId::from_slug("openrouter").map(|p| p.slug()),
            Some("openrouter")
        );
    }

    #[test]
    fn keyless_providers_have_no_env_vars() {
        assert!(ProviderId::Ollama.credentials().env_vars().is_empty());
        assert!(ProviderId::Llamacpp.credentials().env_vars().is_empty());
    }
}
