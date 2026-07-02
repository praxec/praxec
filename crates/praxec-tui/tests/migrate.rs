//! T21 — tests for the `px migrate-agents-from-cli` migration.
//!
//! Two test surfaces:
//!  - `cli_args_to_yaml`: pure translation (the meat of the migration).
//!  - `write_atomic`: filesystem-touching helper. Verified via tempdir.

use praxec_core::model_resolver::ModelsFile;
use praxec_tui::migrate::{cli_args_to_yaml, write_atomic, MigrationError};

fn flag(s: &str) -> String {
    s.to_string()
}

// ── basic positive cases ───────────────────────────────────────────────────

#[test]
fn single_default_binding_round_trips() {
    let yaml = cli_args_to_yaml(&[flag("default=anthropic/claude-sonnet-4-6")]).expect("migrates");
    let parsed = ModelsFile::from_yaml(&yaml).expect("round-trips back through the parser");
    assert_eq!(parsed.default.len(), 1);
    assert_eq!(parsed.default[0].model, "claude-sonnet-4-6");
    assert!(
        parsed.overrides.is_empty(),
        "no override for the literal `default`"
    );
}

#[test]
fn single_override_promotes_to_default_too() {
    // No explicit `default`; the migrator promotes the first --agent
    // as the default so the schema's mandatory `default:` is filled.
    let yaml = cli_args_to_yaml(&[flag("coding=openai/gpt-5")]).expect("migrates");
    let parsed = ModelsFile::from_yaml(&yaml).expect("round-trips");
    assert_eq!(parsed.default.len(), 1);
    assert_eq!(parsed.default[0].model, "gpt-5");
    assert_eq!(parsed.overrides.len(), 1);
}

#[test]
fn multiple_affinity_tier_bindings_round_trip() {
    let yaml = cli_args_to_yaml(&[
        flag("default=anthropic/claude-sonnet-4-6"),
        flag("coding-frontier=openai/gpt-5"),
        flag("reasoning=anthropic/claude-opus-4-7"),
    ])
    .expect("migrates");
    let parsed = ModelsFile::from_yaml(&yaml).expect("round-trips");
    assert_eq!(parsed.default[0].model, "claude-sonnet-4-6");
    assert_eq!(parsed.overrides.len(), 2);
}

// ── last-wins on duplicate names ──────────────────────────────────────────

#[test]
fn duplicate_names_last_wins() {
    // Mirrors agent_config::build_registry. Operator's intent — they
    // typed it twice deliberately.
    let yaml = cli_args_to_yaml(&[
        flag("coding=anthropic/claude-haiku-4-5"),
        flag("coding=openai/gpt-5"),
    ])
    .expect("migrates");
    let parsed = ModelsFile::from_yaml(&yaml).expect("round-trips");
    // The promoted default is the FIRST CLI arg by build order, even
    // though `coding` was redefined later — we promote based on CLI
    // order, not by-name-resolution order.
    assert!(parsed
        .overrides
        .iter()
        .any(|(_, v)| v.iter().any(|b| b.model == "gpt-5")));
}

// ── error cases ────────────────────────────────────────────────────────────

#[test]
fn no_agents_errors_clearly() {
    let err = cli_args_to_yaml(&[]).expect_err("empty list");
    assert!(matches!(err, MigrationError::NoAgents));
}

#[test]
fn unmappable_name_lists_every_offender() {
    let err = cli_args_to_yaml(&[
        flag("planner=anthropic/claude-sonnet-4-6"),
        flag("critic=openai/gpt-5"),
        flag("default=anthropic/claude-opus-4-7"),
    ])
    .expect_err("planner + critic don't map to delegate schema");
    match err {
        MigrationError::UnmappableNames(names) => {
            // Both bad names surface together so the operator sees the
            // full set at once.
            assert_eq!(names.len(), 2);
            assert!(names.contains(&"planner".to_string()));
            assert!(names.contains(&"critic".to_string()));
        }
        other => panic!("unexpected variant: {other:?}"),
    }
}

#[test]
fn unknown_provider_names_offender() {
    let err =
        cli_args_to_yaml(&[flag("coding=mistral/medium")]).expect_err("unknown provider rejected");
    match err {
        MigrationError::UnknownProvider { name, provider, .. } => {
            assert_eq!(name, "coding");
            assert_eq!(provider, "mistral");
        }
        other => panic!("unexpected variant: {other:?}"),
    }
}

#[test]
fn malformed_arg_passes_through_parse_error() {
    let err = cli_args_to_yaml(&[flag("no-equals-sign")]).expect_err("malformed");
    assert!(matches!(err, MigrationError::Parse(_)));
}

// ── atomic write ──────────────────────────────────────────────────────────

#[test]
fn write_atomic_creates_parent_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("nested/path/models.yaml");
    write_atomic("version: 1\n", &target).expect("writes");
    assert!(target.exists());
    let read_back = std::fs::read_to_string(&target).unwrap();
    assert_eq!(read_back, "version: 1\n");
}

#[test]
fn write_atomic_overwrites_existing() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("models.yaml");
    std::fs::write(&target, "old content\n").unwrap();
    write_atomic("new content\n", &target).expect("writes");
    let read_back = std::fs::read_to_string(&target).unwrap();
    assert_eq!(read_back, "new content\n");
}

#[test]
fn write_atomic_no_tmp_leftover_after_success() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("models.yaml");
    write_atomic("version: 1\n", &target).unwrap();
    let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|r| r.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
        .collect();
    assert!(leftovers.is_empty(), "no tmp.{{pid}} leftovers");
}

#[test]
fn end_to_end_migrate_and_validate() {
    // The complete migration loop: take CLI flags → write to disk →
    // load through the resolver → assert it parses.
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("models.yaml");
    let yaml = cli_args_to_yaml(&[
        flag("default=anthropic/claude-sonnet-4-6"),
        flag("coding-frontier=openai/gpt-5"),
    ])
    .expect("migrates");
    write_atomic(&yaml, &target).expect("writes");
    ModelsFile::from_path(&target).expect("on-disk file parses through the real loader");
}
