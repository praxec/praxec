//! The chat LLM configuration (ADR-0005, Increment 3).
//!
//! The cockpit's chat runs on the out-of-band Mission Control LLM. The first-run
//! **chat gate** (`crate::ui::chat_setup`) is recommendation-first, mirroring the
//! embedding gate: collect the providers you have, then recommend the single best
//! conductor model **for your stance** (`crate::priorities`) — surfacing
//! tool-calling (a hard requirement), capability, speed, reasoning effort and the
//! cost magnitude at an adjustable requests/day. This module holds the resulting
//! [`ChatModel`], the provider-key helpers, and detection of existing praxec
//! config (`models.yaml` + `providers.env`) so a usable LLM skips the gate.

use std::path::PathBuf;

use praxec_core::model_resolver::ModelsFile;
use praxec_core::provider_keys;
use praxec_core::providers::ProviderId;

/// The vendors (SDKs) the chat can run on. These strings equal the `ProviderId`
/// slugs, so they map straight to the catalog.
pub const VENDORS: &[&str] = &["anthropic", "openai", "gemini", "openrouter", "ollama"];

/// The configured chat model: a vendor (SDK) + a model id + the chosen reasoning
/// effort. The API key is not held here — it lives in `providers.env` + the
/// process env.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatModel {
    pub vendor: String,
    pub model: String,
    /// Reasoning-effort level (rig's `ReasoningEffort` levels; default "medium").
    /// More effort = more reasoning tokens + latency, surfaced in the gate.
    pub reasoning_effort: String,
}

/// True if `vendor` is keyless, or its primary key is already available — in
/// the process env or the `providers.env` file. Drives both the gate's
/// skip-the-key-step decision and detection.
pub fn has_key(vendor: &str) -> bool {
    let Some(pid) = ProviderId::from_slug(vendor) else {
        return false;
    };
    let Some(primary) = pid.credentials().primary() else {
        return true; // keyless (e.g. ollama)
    };
    if std::env::var(primary).is_ok() {
        return true;
    }
    let Ok(path) = provider_keys::resolve_path() else {
        return false;
    };
    provider_keys::read(&path)
        .map(|m| m.contains_key(primary))
        .unwrap_or(false)
}

/// Persist a freshly-entered key for `vendor`: atomically to `providers.env`
/// (0600) and into the live process env so this session's LLM client can read
/// it. Best-effort; a keyless or unknown vendor is a no-op.
pub fn store_key(vendor: &str, key: &str) {
    let Some(pid) = ProviderId::from_slug(vendor) else {
        return;
    };
    let Some(primary) = pid.credentials().primary() else {
        return;
    };
    if let Ok(path) = provider_keys::resolve_path() {
        let _ = provider_keys::set_var(&path, primary, key);
    }
    // SAFETY: set synchronously from the UI thread during first-run setup,
    // before any task that reads provider env vars is spawned.
    unsafe { std::env::set_var(primary, key) };
}

/// The result of probing existing praxec config for a usable chat model:
/// either a complete model+key (skip the gate) or nothing usable (run it).
/// `None` covers a missing file, a non-gate vendor, or a configured model whose
/// key is absent — in every case the recommendation-first gate takes over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detected {
    /// Model + key both present — skip the gate entirely.
    Complete(ChatModel),
    /// Nothing usable — run the chat gate.
    None,
}

/// Probe real config: the project then user `models.yaml`, plus the
/// provider-keys file / env. The pure core is [`detect_from`] (testable).
pub fn detect() -> Detected {
    detect_from(load_models().as_ref(), has_key)
}

/// Pure detection over an already-loaded models file + a key-presence predicate.
/// Skips the gate only when the default binding is one of the gate [`VENDORS`]
/// *and* its key is present; everything else runs the gate.
pub fn detect_from(models: Option<&ModelsFile>, key_present: impl Fn(&str) -> bool) -> Detected {
    let Some(models) = models else {
        return Detected::None;
    };
    let Some(binding) = models.default.first() else {
        return Detected::None;
    };
    let vendor = binding.provider.display_name();
    if !VENDORS.contains(&vendor) || !key_present(vendor) {
        return Detected::None;
    }
    Detected::Complete(ChatModel {
        vendor: vendor.to_string(),
        model: binding.model.clone(),
        // rig's default effort; the user can retune via Settings → Chat model.
        reasoning_effort: "medium".to_string(),
    })
}

/// Load `models.yaml`, preferring the project file (`.praxec/models.yaml`)
/// over the user file (`~/.praxec/models.yaml`).
fn load_models() -> Option<ModelsFile> {
    let mut candidates: Vec<PathBuf> = vec![PathBuf::from(".praxec/models.yaml")];
    if let Some(dir) = dirs::home_dir() {
        candidates.push(dir.join(".praxec").join("models.yaml"));
    }
    candidates
        .iter()
        .find_map(|p| ModelsFile::from_path(p).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn models(yaml: &str) -> ModelsFile {
        ModelsFile::from_yaml(yaml).expect("valid test models.yaml")
    }

    const OPENAI_DEFAULT: &str = "\
version: 1
default:
  - provider:
      name: openai
    model: gpt-5
";

    #[test]
    fn keyless_vendor_always_has_a_key() {
        assert!(has_key("ollama"));
    }

    #[test]
    fn detect_complete_when_model_and_key_present() {
        let mf = models(OPENAI_DEFAULT);
        let d = detect_from(Some(&mf), |_| true);
        assert_eq!(
            d,
            Detected::Complete(ChatModel {
                vendor: "openai".into(),
                model: "gpt-5".into(),
                reasoning_effort: "medium".into(),
            })
        );
    }

    #[test]
    fn detect_none_when_model_present_but_no_key() {
        let mf = models(OPENAI_DEFAULT);
        // A configured model without its key runs the gate (which collects keys).
        assert_eq!(detect_from(Some(&mf), |_| false), Detected::None);
    }

    #[test]
    fn detect_none_without_a_models_file() {
        assert_eq!(detect_from(None, |_| true), Detected::None);
    }

    #[test]
    fn detect_none_for_a_non_gate_vendor() {
        // A default bound to a provider outside the five gate vendors can't
        // drive the gate — fall through to the full flow.
        let mf = models(
            "\
version: 1
default:
  - provider:
      name: bedrock
    model: claude-via-bedrock
",
        );
        assert_eq!(detect_from(Some(&mf), |_| true), Detected::None);
    }
}
