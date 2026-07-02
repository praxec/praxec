//! Migration regression lint: SPEC §5 legacy verb examples must not reappear
//! in the codebase. The new closed `Verb` enum (SPEC §5.4.1) replaced
//! `apply`/`check`/`avoid`/`follow`/etc. If a future change adds a fixture
//! that uses one of the old verbs, this test fails fast.
//!
//! Note: `check` is also a legitimate state name in some legacy test
//! fixtures (e.g. `guidance.rs` uses `"check"` as a workflow state); the
//! lint targets *verb-position* usage only.

use std::fs;
use std::path::{Path, PathBuf};

const LEGACY_VERBS: &[&str] = &[
    "apply",
    "check",
    "avoid",
    "follow",
    "enforce",
    "suggest",
    "configure",
    "document",
];

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR points at this crate; workspace root is two up.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root is two parents above CARGO_MANIFEST_DIR")
        .to_path_buf()
}

/// Recursively walk a directory, returning file paths matching the allowed
/// extensions. Skips `target/`, `.git/`, and `node_modules/` for sanity.
fn walk(root: &Path, exts: &[&str]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let p = entry.path();
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with('.') || name == "target" || name == "node_modules" {
                continue;
            }
            if p.is_dir() {
                stack.push(p);
            } else if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
                if exts.contains(&ext) {
                    out.push(p);
                }
            }
        }
    }
    out
}

/// Match a verb-position pattern: `verb: <token>` or `"verb": "<token>"`.
/// Avoids matching unrelated occurrences of the legacy strings (state names,
/// English prose in comments).
fn line_uses_legacy_verb_in_verb_position(line: &str) -> Option<&'static str> {
    for &legacy in LEGACY_VERBS {
        let patterns = [
            format!("verb: {legacy}"),
            format!("verb: \"{legacy}\""),
            format!("\"verb\": \"{legacy}\""),
            format!("\"verb\":\"{legacy}\""),
        ];
        for p in &patterns {
            if line.contains(p.as_str()) {
                return Some(legacy);
            }
        }
    }
    None
}

#[test]
fn no_legacy_verb_in_verb_position_across_workspace() {
    let root = workspace_root();
    let files = walk(&root, &["rs", "yaml", "yml", "md", "json"]);
    let self_path: PathBuf = PathBuf::from(file!());
    let self_name = self_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("migration_check.rs");

    let mut violations: Vec<String> = Vec::new();
    for path in files {
        // This lint file itself names the legacy verbs verbatim; skip it.
        if path.file_name().and_then(|n| n.to_str()) == Some(self_name) {
            continue;
        }
        // spec.md describes legacy verbs as "what we cut", so it's allowed
        // to mention them in prose. Allow occurrences ONLY within the
        // "Considered and cut" §4 area is too brittle; for now allow the
        // entire spec.md. Code/fixture files are the regression risk.
        if path.file_name().and_then(|n| n.to_str()) == Some("spec.md") {
            continue;
        }
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for (lineno, line) in content.lines().enumerate() {
            if let Some(legacy) = line_uses_legacy_verb_in_verb_position(line) {
                violations.push(format!(
                    "{}:{}: legacy verb '{}' in verb position",
                    path.strip_prefix(&root).unwrap_or(&path).display(),
                    lineno + 1,
                    legacy
                ));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "Found legacy verb usage in verb position. SPEC §5.4.1 closed the verb \
         vocabulary to 8 cognitive verbs; replace these:\n  {}\n",
        violations.join("\n  ")
    );
}
