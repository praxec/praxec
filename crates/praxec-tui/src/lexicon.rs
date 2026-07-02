//! SPEC §30.10.7 — `px lexicon` CLI subcommand suite.
//!
//! Provides operator-facing out-of-band management of the lexicon:
//!
//! ```text
//! px lexicon define <term> --definition-short "..." [options]
//! px lexicon alias <term> --add <alias>
//! px lexicon cancel <term>
//! px lexicon list [--bounded-context X]
//! px lexicon pending
//! ```
//!
//! ## Storage note
//!
//! The current lexicon backend is an in-memory overlay over the config-file
//! lexicon block (`praxec.yaml`). The `define`, `alias`, and `cancel`
//! subcommands write to that in-memory overlay. **These mutations do not persist
//! across binary invocations.** To make definitions permanent, edit the
//! `lexicon:` block in `praxec.yaml` directly and reload.
//!
//! The `list` and `pending` subcommands read from the _resolved_ config file
//! (loaded via `--config` or `$PRAXEC_CONFIG`). They reflect the
//! file-persisted state, not any in-memory overlay.

use std::collections::{HashMap, HashSet};
use std::process::ExitCode;

use anyhow::Result;
use clap::Subcommand;
use serde_json::{json, Map, Value};

use praxec_core::model::Principal;

/// Returns a `Principal` representing CLI invocation (operator / human).
fn cli_principal() -> Principal {
    Principal {
        subject: "cli".to_string(),
        roles: vec!["human".to_string()],
        permissions: Vec::new(),
    }
}

// ── Clap types ──────────────────────────────────────────────────────────────

/// `px lexicon <subcommand>` — out-of-band lexicon management.
#[derive(Subcommand, Debug)]
pub enum LexiconCmd {
    /// Define or redefine a lexicon term.
    ///
    /// Creates (or overwrites) a term in the in-memory overlay. To persist,
    /// edit the `lexicon:` block in `praxec.yaml` and reload.
    Define(DefineArgs),
    /// Add one alias to an existing lexicon term.
    Alias(AliasArgs),
    /// Drop a PENDING_DEFINITION placeholder (cancel it without defining).
    ///
    /// Exits nonzero when the term is not a pending placeholder.
    Cancel(CancelArgs),
    /// List all lexicon entries in the config file.
    ///
    /// Reads from `--config` / `$PRAXEC_CONFIG` and emits one JSON object
    /// per line (JSON Lines), each carrying `{"term": ..., ...entry-fields}`.
    List(ListArgs),
    /// List only PENDING_DEFINITION placeholder entries.
    ///
    /// Same reading source as `list`. Emits one JSON object per line.
    Pending(PendingArgs),
}

#[derive(clap::Args, Debug)]
pub struct DefineArgs {
    /// The canonical term name (e.g. `evidence-pack`).
    pub term: String,

    /// One-sentence definition (required).
    #[arg(long)]
    pub definition_short: String,

    /// Optional extended prose definition.
    #[arg(long)]
    pub definition_long: Option<String>,

    /// Optional DDD bounded context.
    #[arg(long)]
    pub bounded_context: Option<String>,

    /// Comma-separated aliases (e.g. `ep,evpack`).
    #[arg(long)]
    pub aliases: Option<String>,

    /// Governance level: `human-only` (default) or `agent-may-propose`.
    #[arg(long)]
    pub governance: Option<String>,

    /// Path to `praxec.yaml` (config). When omitted, falls back to
    /// `$PRAXEC_CONFIG`. Required to populate the overlay from the
    /// base lexicon before writing.
    #[arg(long, env = "PRAXEC_CONFIG")]
    pub config: Option<String>,
}

#[derive(clap::Args, Debug)]
pub struct AliasArgs {
    /// The existing canonical term to attach the alias to.
    pub term: String,

    /// The alias to add.
    #[arg(long)]
    pub add: String,

    /// Path to `praxec.yaml` for overlay initialisation.
    #[arg(long, env = "PRAXEC_CONFIG")]
    pub config: Option<String>,
}

#[derive(clap::Args, Debug)]
pub struct CancelArgs {
    /// The pending subject term to cancel.
    pub term: String,

    /// Path to `praxec.yaml` for overlay initialisation.
    #[arg(long, env = "PRAXEC_CONFIG")]
    pub config: Option<String>,
}

#[derive(clap::Args, Debug)]
pub struct ListArgs {
    /// Filter by bounded context.
    #[arg(long)]
    pub bounded_context: Option<String>,

    /// Path to `praxec.yaml` (config).
    #[arg(long, env = "PRAXEC_CONFIG")]
    pub config: Option<String>,
}

#[derive(clap::Args, Debug)]
pub struct PendingArgs {
    /// Path to `praxec.yaml` (config).
    #[arg(long, env = "PRAXEC_CONFIG")]
    pub config: Option<String>,
}

// ── Entry point ─────────────────────────────────────────────────────────────

/// Dispatch `px lexicon <subcommand>`.
pub fn run(cmd: LexiconCmd) -> Result<ExitCode> {
    match cmd {
        LexiconCmd::Define(args) => run_define(args),
        LexiconCmd::Alias(args) => run_alias(args),
        LexiconCmd::Cancel(args) => run_cancel(args),
        LexiconCmd::List(args) => run_list(args),
        LexiconCmd::Pending(args) => run_pending(args),
    }
}

// ── Subcommand implementations ───────────────────────────────────────────────

fn run_define(args: DefineArgs) -> Result<ExitCode> {
    let principal = cli_principal();

    // Parse aliases from comma-separated string.
    let aliases: Option<Vec<String>> = args.aliases.as_deref().map(|s| {
        s.split(',')
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty())
            .collect()
    });

    // Validate definition_short.
    if args.definition_short.trim().is_empty() {
        anyhow::bail!("INVALID_LEXICON_ENTRY: --definition-short must be non-empty");
    }

    // Build the entry using core helper.
    let mut entry = praxec_core::lexicon::build_entry(
        &args.definition_short,
        args.bounded_context.as_deref(),
        None,
        args.governance.as_deref(),
        None, // TUI does not have an embedder context; embeddings are a server-side concern
    )?;

    // Add optional fields to the entry object.
    if let Some(long) = &args.definition_long {
        if let Some(obj) = entry.as_object_mut() {
            obj.insert("definition_long".to_string(), json!(long));
        }
    }
    if let Some(aliases_vec) = &aliases {
        if !aliases_vec.is_empty() {
            if let Some(obj) = entry.as_object_mut() {
                obj.insert("aliases".to_string(), json!(aliases_vec));
            }
        }
    }

    // Build in-memory overlay (initialised from config base, if available).
    let mut overlay = load_overlay_base(args.config.as_deref())?;
    let mut pending = load_pending_set(args.config.as_deref())?;

    // Governance check: is this a human-only existing entry being overwritten?
    // Since CLI uses a human principal, this always passes (humans bypass the gate).
    let is_human = principal.is_human();
    let merged = build_merged_definition(&overlay, args.config.as_deref())?;
    if !is_human {
        if let Err(msg) = praxec_core::lexicon::define_allowed(&merged, &args.term, false) {
            anyhow::bail!("{msg}");
        }
    }

    // Write to overlay.
    overlay.insert(args.term.clone(), entry.clone());

    // Remove from pending if it was there.
    pending.remove(&args.term);

    // Emit audit line to stderr (same info as runtime audit event).
    let event = json!({
        "event": "lexicon.defined",
        "actor": principal.subject,
        "term": args.term,
        "bounded_context": args.bounded_context,
        "by_human": is_human,
    });
    eprintln!("audit: {event}");

    // Emit result JSON to stdout.
    let result = json!({
        "term": args.term,
        "entry": entry,
        "persisted_to": "overlay (in-memory only; edit praxec.yaml to persist)"
    });
    println!("{result}");
    Ok(ExitCode::SUCCESS)
}

fn run_alias(args: AliasArgs) -> Result<ExitCode> {
    let principal = cli_principal();

    // Build in-memory overlay.
    let mut overlay = load_overlay_base(args.config.as_deref())?;
    let merged = build_merged_definition(&overlay, args.config.as_deref())?;

    // Look up the target term.
    let lib = merged
        .get("_lexiconLibrary")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    let existing = lib.get(&args.term).cloned();
    let mut entry = match existing {
        Some(e) if e.get("state").and_then(Value::as_str) != Some("PENDING_DEFINITION") => {
            e.as_object().cloned().unwrap_or_default()
        }
        _ => {
            anyhow::bail!(
                "LEXICON_ENTRY_NOT_FOUND: no real entry for term '{}'. \
                 lexicon alias requires an existing authored entry as target.",
                args.term
            );
        }
    };

    // Collision check.
    let target_ctx = entry
        .get("bounded_context")
        .and_then(Value::as_str)
        .unwrap_or("");
    match praxec_core::lexicon::build_combined_index(&lib, target_ctx) {
        Err(collision_msg) => {
            anyhow::bail!("LEXICON_ALIAS_COLLISION: {collision_msg}");
        }
        Ok(index) => {
            if let Some(_existing_entry) = index.get(args.add.as_str()) {
                anyhow::bail!(
                    "LEXICON_ALIAS_COLLISION: within bounded_context '{}', key '{}' \
                     is already claimed. (SPEC §30.10.1)",
                    target_ctx,
                    args.add
                );
            }
        }
    }

    // Append alias.
    let current_aliases = entry
        .get("aliases")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut new_aliases = current_aliases;
    let alias_val = Value::String(args.add.clone());
    if !new_aliases.contains(&alias_val) {
        new_aliases.push(alias_val);
    }
    entry.insert("aliases".to_string(), Value::Array(new_aliases));

    // Persist into overlay.
    overlay.insert(args.term.clone(), Value::Object(entry));

    // Emit audit line to stderr.
    let event = json!({
        "event": "lexicon.alias_added",
        "actor": principal.subject,
        "term": args.term,
        "alias": args.add,
    });
    eprintln!("audit: {event}");

    println!(
        "{}",
        json!({
            "term": args.term,
            "alias_added": args.add,
            "persisted_to": "overlay (in-memory only; edit praxec.yaml to persist)"
        })
    );
    Ok(ExitCode::SUCCESS)
}

fn run_cancel(args: CancelArgs) -> Result<ExitCode> {
    let principal = cli_principal();

    // Load pending subjects from the resolved config.
    let mut pending = load_pending_set(args.config.as_deref())?;

    if !pending.contains(&args.term) {
        anyhow::bail!(
            "INVALID_RESOLUTION: subject '{}' is not a pending placeholder. \
             Cancel applies only to PENDING_DEFINITION subjects. (SPEC §30.10.9)",
            args.term
        );
    }

    pending.remove(&args.term);

    // Emit audit line to stderr.
    let event = json!({
        "event": "lexicon.pending_cancelled",
        "actor": principal.subject,
        "term": args.term,
        "cancelled_by": principal.subject,
    });
    eprintln!("audit: {event}");

    println!(
        "{}",
        json!({
            "cancelled": args.term,
            "persisted_to": "pending_subjects (in-memory only)"
        })
    );
    Ok(ExitCode::SUCCESS)
}

fn run_list(args: ListArgs) -> Result<ExitCode> {
    let cfg = require_config(args.config.as_deref(), "lexicon list")?;

    // Read the authored lexicon block from the config.
    let lexicon = cfg
        .pointer("/lexicon")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    let bc_filter = args.bounded_context.as_deref();
    let mut count = 0usize;
    for (term, entry) in &lexicon {
        // Bounded context filter.
        if let Some(filter_ctx) = bc_filter {
            let entry_ctx = entry
                .get("bounded_context")
                .and_then(Value::as_str)
                .unwrap_or("");
            if entry_ctx != filter_ctx {
                continue;
            }
        }
        let mut obj = entry.as_object().cloned().unwrap_or_default();
        obj.insert("term".to_string(), json!(term));
        println!("{}", serde_json::to_string(&Value::Object(obj))?);
        count += 1;
    }

    eprintln!("{count} lexicon entry/entries");
    Ok(ExitCode::SUCCESS)
}

fn run_pending(args: PendingArgs) -> Result<ExitCode> {
    let cfg = require_config(args.config.as_deref(), "lexicon pending")?;

    let pending = praxec_core::lexicon::pending_subjects_from_resolved(&cfg);

    for term in &pending {
        println!(
            "{}",
            serde_json::to_string(&json!({
                "term": term,
                "state": "PENDING_DEFINITION"
            }))?
        );
    }

    eprintln!("{} pending subject(s)", pending.len());
    Ok(ExitCode::SUCCESS)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Load and resolve the config file at the given path (or `$PRAXEC_CONFIG`).
/// Returns an error when no path is provided or the file cannot be resolved.
fn require_config(config_arg: Option<&str>, subcommand: &str) -> Result<Value> {
    let path = config_arg
        .map(String::from)
        .or_else(|| std::env::var("PRAXEC_CONFIG").ok());
    match path {
        None => anyhow::bail!(
            "lexicon {subcommand}: no config path. Pass --config or set $PRAXEC_CONFIG."
        ),
        Some(p) => {
            let path = std::path::Path::new(&p);
            if !path.exists() {
                anyhow::bail!("config file not found: {p}");
            }
            let raw = std::fs::read_to_string(path)?;
            let value: Value = serde_yaml::from_str(&raw)?;
            praxec_core::config::resolve(value).map_err(|e| anyhow::anyhow!("{e}"))
        }
    }
}

/// Build an initial in-memory overlay from the config's base lexicon block.
/// Returns an empty map when no config path is given or the file lacks a
/// `lexicon:` block (this is fine — `define` can populate from nothing).
fn load_overlay_base(config_arg: Option<&str>) -> Result<HashMap<String, Value>> {
    match config_arg {
        None => Ok(HashMap::new()),
        Some(p) => {
            let path = std::path::Path::new(p);
            if !path.exists() {
                return Ok(HashMap::new());
            }
            let raw = std::fs::read_to_string(path)?;
            let value: Value = serde_yaml::from_str(&raw)?;
            let resolved = praxec_core::config::resolve(value)?;
            let map: HashMap<String, Value> = resolved
                .pointer("/lexicon")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .collect();
            Ok(map)
        }
    }
}

/// Load the set of PENDING_DEFINITION subjects from the resolved config.
fn load_pending_set(config_arg: Option<&str>) -> Result<HashSet<String>> {
    let set = match config_arg {
        None => HashSet::new(),
        Some(p) => {
            let path = std::path::Path::new(p);
            if !path.exists() {
                return Ok(HashSet::new());
            }
            let raw = std::fs::read_to_string(path)?;
            let value: Value = serde_yaml::from_str(&raw)?;
            let resolved = praxec_core::config::resolve(value)?;
            praxec_core::lexicon::pending_subjects_from_resolved(&resolved)
                .into_iter()
                .collect()
        }
    };
    Ok(set)
}

/// Build the merged definition value (base ∪ overlay) in the shape expected
/// by `praxec_core::lexicon::*` helpers: `{ "_lexiconLibrary": { ... } }`.
fn build_merged_definition(
    overlay: &HashMap<String, Value>,
    config_arg: Option<&str>,
) -> Result<Value> {
    let mut base: Map<String, Value> = match config_arg {
        None => Map::new(),
        Some(p) => {
            let path = std::path::Path::new(p);
            if !path.exists() {
                Map::new()
            } else {
                let raw = std::fs::read_to_string(path)?;
                let value: Value = serde_yaml::from_str(&raw)?;
                let resolved = praxec_core::config::resolve(value)?;
                resolved
                    .pointer("/lexicon")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default()
            }
        }
    };
    for (k, v) in overlay {
        base.insert(k.clone(), v.clone());
    }
    Ok(json!({ "_lexiconLibrary": base }))
}
