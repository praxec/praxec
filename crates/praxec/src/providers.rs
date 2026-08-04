//! `praxec providers` — native provider-key management.
//!
//! Key management is a CORE setup task, so it lives in the gateway binary
//! everyone installs — not stranded in the optional `px` TUI or a POSIX-only
//! shell script (this works on Windows too). A thin CLI over the SHARED
//! [`praxec_core::provider_keys`] backend that `praxec init` already uses, so
//! `init` (capture one key), `providers` (manage them), and `doctor` (verify)
//! read and write the exact same `providers.env`.

use std::path::Path;

use anyhow::Context;
use praxec_core::provider_keys::{mask_value, read, remove_provider, resolve_path, set_var};
use praxec_core::providers::ProviderId;

use crate::gateway_config::ProvidersCommand;

/// Dispatch `praxec providers <cmd>`. All commands operate on the resolved
/// provider-keys file (`$PRAXEC_PROVIDER_KEYS_FILE`, else
/// `~/.config/praxec/providers.env`, else legacy `~/.praxec/providers.env`).
pub(crate) fn run(command: ProvidersCommand) -> anyhow::Result<()> {
    let path = resolve_path()?;
    match command {
        ProvidersCommand::Path => {
            println!("{}", path.display());
            Ok(())
        }
        ProvidersCommand::List => list(&path),
        ProvidersCommand::Remove { provider } => {
            let p = provider_from_slug(&provider)?;
            remove_provider(&path, p)?;
            eprintln!("removed {} keys from {}", p.display(), path.display());
            Ok(())
        }
        ProvidersCommand::Set {
            provider,
            key_stdin,
            from_env,
        } => match provider {
            Some(slug) => set_one(&path, provider_from_slug(&slug)?, key_stdin, from_env),
            None => interactive(&path),
        },
    }
}

fn provider_from_slug(slug: &str) -> anyhow::Result<ProviderId> {
    ProviderId::from_slug(slug)
        .ok_or_else(|| anyhow::anyhow!("unknown provider '{slug}'. Valid: {}", valid_slugs()))
}

fn valid_slugs() -> String {
    ProviderId::ALL
        .iter()
        .map(|p| p.slug())
        .collect::<Vec<_>>()
        .join(", ")
}

/// `providers list` — every configured provider with its key(s) masked.
fn list(path: &Path) -> anyhow::Result<()> {
    let vars = read(path)?;
    if vars.is_empty() {
        println!("(no provider keys configured at {})", path.display());
        return Ok(());
    }
    println!("{}:", path.display());
    for provider in ProviderId::ALL {
        let env_vars = provider.credentials().env_vars();
        if env_vars.is_empty() {
            continue; // local provider — nothing to display
        }
        let mut any = false;
        for k in &env_vars {
            if let Some(v) = vars.get(*k) {
                if !any {
                    println!("  {}:", provider.display());
                    any = true;
                }
                println!("    {k}={}", mask_value(v));
            }
        }
    }
    Ok(())
}

/// Read one env var's value: from the environment (`--from-env`), stdin
/// (`--key-stdin`), or a no-echo prompt (default).
fn read_value(
    env_var: &str,
    provider: ProviderId,
    key_stdin: bool,
    from_env: bool,
) -> anyhow::Result<String> {
    if from_env {
        return Ok(std::env::var(env_var)
            .unwrap_or_default()
            .trim()
            .to_string());
    }
    if key_stdin {
        let mut s = String::new();
        std::io::stdin().read_line(&mut s)?;
        return Ok(s.trim().to_string());
    }
    Ok(
        rpassword::prompt_password(format!("{} ({env_var}): ", provider.display()))?
            .trim()
            .to_string(),
    )
}

/// `providers set --provider <slug>` — set that provider's key(s). A multi-var
/// provider (e.g. `bedrock`) consumes one value per env var, in declaration order.
fn set_one(
    path: &Path,
    provider: ProviderId,
    key_stdin: bool,
    from_env: bool,
) -> anyhow::Result<()> {
    if provider.credentials().env_vars().is_empty() {
        eprintln!(
            "{} is a local provider and needs no API key.",
            provider.display()
        );
        return Ok(());
    }
    let mut any = false;
    for env_var in provider.credentials().env_vars() {
        let value = read_value(env_var, provider, key_stdin, from_env)?;
        if value.is_empty() {
            eprintln!("(empty value for {env_var} — skipped)");
            continue;
        }
        set_var(path, env_var, &value).with_context(|| format!("writing {}", path.display()))?;
        any = true;
    }
    if any {
        eprintln!("saved {} keys to {}", provider.display(), path.display());
    } else {
        eprintln!("no keys written for {}", provider.display());
    }
    Ok(())
}

/// `providers set` (no `--provider`) — walk every key-bearing provider, no-echo.
fn interactive(path: &Path) -> anyhow::Result<()> {
    println!("praxec provider keys → {}", path.display());
    println!("(press Enter to skip a provider; values are not echoed)");
    for provider in ProviderId::ALL {
        let env_vars = provider.credentials().env_vars();
        if env_vars.is_empty() {
            continue;
        }
        println!();
        println!("== {} ({}) ==", provider.display(), provider.slug());
        let mut any = false;
        for env_var in &env_vars {
            let value = rpassword::prompt_password(format!("  {env_var}: "))?
                .trim()
                .to_string();
            if value.is_empty() {
                continue;
            }
            set_var(path, env_var, &value)?;
            any = true;
        }
        if any {
            eprintln!("  saved {} keys", provider.display());
        }
    }
    Ok(())
}
