//! The `set-provider-keys` CLI surface.
//!
//! The pure file backend (path resolution, atomic read/write, masking)
//! lives in [`praxec_core::provider_keys`]; this module is the clap
//! command that drives it (interactive walk, `--list`, `--provider`,
//! `--remove`, `--path`) plus the startup env-load wrapper.

use std::path::Path;
use std::process::ExitCode;

pub use praxec_core::provider_keys::load_into_env_if_present;
use praxec_core::provider_keys::{mask_value, read, remove_provider, resolve_path, set_var};
use praxec_core::providers::ProviderId;

/// CLI args for `px set-provider-keys`.
///
/// Without flags: interactive walk of every supported provider.
/// `--list`: show configured providers with masked values.
/// `--provider <name>`: set one provider; reads value from stdin
/// (with `--stdin`) or via no-echo prompt (without `--stdin`).
/// Multi-var providers (e.g. `bedrock` → `AWS_ACCESS_KEY_ID`,
/// `AWS_SECRET_ACCESS_KEY`, `AWS_REGION`) consume one stdin line per
/// env var in declaration order.
/// `--remove <name>`: clear one provider's vars.
/// `--path`: print the resolved file path and exit.
#[derive(clap::Args, Debug)]
pub struct SetProviderKeysArgs {
    #[arg(long, value_name = "PROVIDER")]
    pub provider: Option<String>,

    #[arg(long, conflicts_with_all = ["provider", "remove", "path"])]
    pub list: bool,

    #[arg(long, value_name = "PROVIDER", conflicts_with_all = ["provider", "list", "path"])]
    pub remove: Option<String>,

    #[arg(long, conflicts_with_all = ["provider", "list", "remove"])]
    pub path: bool,

    #[arg(long, requires = "provider")]
    pub stdin: bool,
}

pub fn run(args: SetProviderKeysArgs) -> anyhow::Result<ExitCode> {
    let path = resolve_path()?;

    if args.path {
        println!("{}", path.display());
        return Ok(ExitCode::SUCCESS);
    }

    if args.list {
        return run_list(&path);
    }

    if let Some(slug) = args.remove.as_deref() {
        let provider = ProviderId::from_slug(slug).ok_or_else(|| {
            anyhow::anyhow!("unknown provider '{slug}'. Valid: {}", valid_slugs())
        })?;
        remove_provider(&path, provider)?;
        eprintln!(
            "removed {} keys from {}",
            provider.display(),
            path.display()
        );
        return Ok(ExitCode::SUCCESS);
    }

    if let Some(slug) = args.provider.as_deref() {
        let provider = ProviderId::from_slug(slug).ok_or_else(|| {
            anyhow::anyhow!("unknown provider '{slug}'. Valid: {}", valid_slugs())
        })?;
        return run_set_one(&path, provider, args.stdin);
    }

    run_interactive(&path)
}

fn valid_slugs() -> String {
    ProviderId::ALL
        .iter()
        .map(|p| p.slug())
        .collect::<Vec<_>>()
        .join(", ")
}

fn run_list(path: &Path) -> anyhow::Result<ExitCode> {
    let vars = read(path)?;
    if vars.is_empty() {
        println!("(no provider keys configured at {})", path.display());
        return Ok(ExitCode::SUCCESS);
    }
    println!("{}:", path.display());
    for provider in ProviderId::ALL {
        let env_vars = provider.credentials().env_vars();
        if env_vars.is_empty() {
            continue; // local provider — no key to display
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
    Ok(ExitCode::SUCCESS)
}

fn run_set_one(path: &Path, provider: ProviderId, from_stdin: bool) -> anyhow::Result<ExitCode> {
    if provider.credentials().env_vars().is_empty() {
        eprintln!(
            "{} is a local provider and needs no API key.",
            provider.display()
        );
        return Ok(ExitCode::SUCCESS);
    }
    let mut any = false;
    for env_var in provider.credentials().env_vars() {
        let value = if from_stdin {
            let mut s = String::new();
            std::io::stdin().read_line(&mut s)?;
            s.trim().to_string()
        } else {
            rpassword::prompt_password(format!("{} ({env_var}): ", provider.display()))?
                .trim()
                .to_string()
        };
        if value.is_empty() {
            eprintln!("(empty value for {env_var} — skipped)");
            continue;
        }
        set_var(path, env_var, &value)?;
        any = true;
    }
    if any {
        eprintln!("saved {} keys to {}", provider.display(), path.display());
    } else {
        eprintln!("no keys written for {}", provider.display());
    }
    Ok(ExitCode::SUCCESS)
}

fn run_interactive(path: &Path) -> anyhow::Result<ExitCode> {
    println!("praxec provider keys → {}", path.display());
    println!("(press Enter to skip a provider; values are not echoed)");
    for provider in ProviderId::ALL {
        let env_vars = provider.credentials().env_vars();
        if env_vars.is_empty() {
            continue; // local provider — no key to set
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
    Ok(ExitCode::SUCCESS)
}

// The startup env-load wrapper now lives in `praxec_core::provider_keys`
// (`load_into_env_if_present`, re-exported above) so BOTH the `px` and `praxec`
// binaries share one implementation. The `px` `main()` calls it via this module
// exactly as before.
