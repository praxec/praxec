//! Safe, governed primitive to add/remove a `connections.<name>` entry in a
//! gateway config YAML file.
//!
//! This is FILE-AGNOSTIC and POLICY-FREE: the caller decides which file to
//! mutate (the top-level gateway config, an `include:`d fragment, whatever).
//! This module does not know about `include:` bootstrapping, staged
//! connections, or any other config policy — that lives in `config.rs` and
//! the provision flow. Its only job is: mutate `connections.<name>` in ONE
//! YAML file without ever corrupting it.
//!
//! Correctness properties (the whole point of this module):
//! - **Name-safety**: [`add_connection`] never silently overwrites an
//!   existing connection — a name collision is a hard [`ConfigMutationError::NameExists`].
//! - **Backup**: the original file bytes are copied to `<path>.bak.<backup_label>`
//!   *before* anything is written.
//! - **Atomicity**: the new content is written to a tempfile in the SAME
//!   directory as `path` (rename is only atomic within one filesystem), then
//!   `std::fs::rename`d over `path`. Any error before the rename leaves
//!   `path` untouched and cleans up the tempfile — there is no window where
//!   `path` can be observed half-written.
//! - **Preservation**: the file is parsed → only the `connections` node is
//!   mutated → re-serialized. Every other top-level key and every sibling
//!   connection is preserved as parsed (structurally, not byte-for-byte —
//!   serde_yaml's own re-serialization may reformat comments/spacing, but no
//!   data is lost or altered).
//!
//! Validation scope — **be honest about what this does NOT check**: this
//! module only guarantees the file is well-formed YAML and that
//! `connections` is (still) a mapping after the edit. It does NOT validate
//! the semantic shape of `block` (e.g. that a `kind: mcp` connection has a
//! `command` or `url`), does NOT resolve `include:`s, and does NOT check the
//! result against the full config schema. Full semantic validation happens
//! later, at gateway load time (`config.rs`) and at the provision flow's HITL
//! gate — this primitive's contract stops at "the file is never left
//! corrupt."
//!
//! No `Date`/`Instant`/`SystemTime::now` — forbidden in praxec-core. The
//! `backup_label` is caller-supplied (the server layer stamps a timestamp)
//! so this module stays pure and testable.

use std::path::Path;

use serde_yaml::Value;

/// Errors from a config-mutation operation.
#[derive(Debug, thiserror::Error)]
pub enum ConfigMutationError {
    /// `add_connection` was asked to add a name that already exists under
    /// `connections`. Never silently overwritten.
    #[error("connection '{0}' already exists in connections:")]
    NameExists(String),
    /// Any filesystem error (read, write, rename, backup copy).
    #[error("io error: {0}")]
    Io(String),
    /// The target file's content did not parse as YAML.
    #[error("failed to parse config as YAML: {0}")]
    Parse(String),
    /// The file parsed as YAML, but not in a shape this primitive can work
    /// with (e.g. `connections:` exists but is not a mapping).
    #[error("invalid config shape: {0}")]
    Invalid(String),
}

impl From<std::io::Error> for ConfigMutationError {
    fn from(e: std::io::Error) -> Self {
        ConfigMutationError::Io(e.to_string())
    }
}

/// Add `block` under `connections.<name>` in the YAML file at `path`.
///
/// - `Err(NameExists)` if `name` already exists under `connections` — never
///   silently overwritten. The file and any prior backup are left untouched.
/// - Creates the top-level `connections` mapping if absent.
/// - Backs up the original bytes to `<path>.bak.<backup_label>` before
///   writing anything.
/// - Writes atomically: serializes to `<path>.tmp.<backup_label>` in the
///   same directory, then renames over `path`.
/// - Validates only that the RESULT re-parses as YAML with `connections` as
///   a mapping — see the module header for the full validation-scope
///   disclaimer.
pub fn add_connection(
    path: &Path,
    name: &str,
    block: &Value,
    backup_label: &str,
) -> Result<(), ConfigMutationError> {
    let (original, mut root) = read_and_parse(path)?;
    let conns = connections_mapping_mut(&mut root)?;

    let key = Value::String(name.to_string());
    if conns.contains_key(&key) {
        return Err(ConfigMutationError::NameExists(name.to_string()));
    }
    conns.insert(key, block.clone());

    write_mutated(path, &original, &root, backup_label)
}

/// Remove `connections.<name>` from the YAML file at `path`.
///
/// No-op (`Ok(())`, no backup/write) if the connection is absent — either
/// because `connections:` doesn't exist at all, or the name isn't in it.
/// Otherwise same backup + atomic-write contract as [`add_connection`].
pub fn remove_connection(
    path: &Path,
    name: &str,
    backup_label: &str,
) -> Result<(), ConfigMutationError> {
    let (original, mut root) = read_and_parse(path)?;

    let Some(conns_val) = root.get_mut("connections") else {
        return Ok(()); // no connections: key at all — nothing to remove
    };
    let Value::Mapping(conns) = conns_val else {
        return Err(ConfigMutationError::Invalid(
            "`connections` exists but is not a mapping".to_string(),
        ));
    };
    let key = Value::String(name.to_string());
    if conns.shift_remove(&key).is_none() {
        return Ok(()); // absent — no-op
    }

    write_mutated(path, &original, &root, backup_label)
}

/// Read the target file and parse it as YAML. Returns the raw bytes (for the
/// backup) alongside the parsed root value.
fn read_and_parse(path: &Path) -> Result<(Vec<u8>, Value), ConfigMutationError> {
    let original = std::fs::read(path)?;
    let root: Value =
        serde_yaml::from_slice(&original).map_err(|e| ConfigMutationError::Parse(e.to_string()))?;
    Ok((original, root))
}

/// Get (creating if absent) the `connections` node as a mutable mapping.
/// `root` must be a mapping itself, or become one if it was `Null` (an empty
/// file parses to `Value::Null`).
fn connections_mapping_mut(
    root: &mut Value,
) -> Result<&mut serde_yaml::Mapping, ConfigMutationError> {
    if matches!(root, Value::Null) {
        *root = Value::Mapping(serde_yaml::Mapping::new());
    }
    let Value::Mapping(root_map) = root else {
        return Err(ConfigMutationError::Invalid(
            "config root is not a YAML mapping".to_string(),
        ));
    };
    let entry = root_map
        .entry(Value::String("connections".to_string()))
        .or_insert_with(|| Value::Mapping(serde_yaml::Mapping::new()));
    let Value::Mapping(conns) = entry else {
        return Err(ConfigMutationError::Invalid(
            "`connections` exists but is not a mapping".to_string(),
        ));
    };
    Ok(conns)
}

/// Shared backup + validate-result + atomic-write tail for both operations.
fn write_mutated(
    path: &Path,
    original: &[u8],
    root: &Value,
    backup_label: &str,
) -> Result<(), ConfigMutationError> {
    // Re-serialize and validate the RESULT before touching disk at all: a
    // failure here must leave the original file (and no backup) untouched.
    let serialized =
        serde_yaml::to_string(root).map_err(|e| ConfigMutationError::Invalid(e.to_string()))?;
    let reparsed: Value = serde_yaml::from_str(&serialized)
        .map_err(|e| ConfigMutationError::Invalid(format!("result failed to re-parse: {e}")))?;
    match reparsed.get("connections") {
        Some(Value::Mapping(_)) | None => {}
        Some(_) => {
            return Err(ConfigMutationError::Invalid(
                "result `connections` is not a mapping after mutation".to_string(),
            ));
        }
    }

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let backup_path = parent.join(format!(
        "{}.bak.{backup_label}",
        path.file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("config")
    ));
    let tmp_path = parent.join(format!(
        "{}.tmp.{backup_label}",
        path.file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("config")
    ));

    // Backup BEFORE any write to the real path.
    std::fs::write(&backup_path, original)?;

    // Write the temp file in the SAME directory, then rename atomically.
    let write_result = std::fs::write(&tmp_path, &serialized);
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(ConfigMutationError::Io(e.to_string()));
    }
    if let Err(e) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(ConfigMutationError::Io(e.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_yaml::Mapping;

    fn write_fixture(dir: &Path, name: &str, contents: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn read_str(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    fn backup_path(path: &Path, label: &str) -> std::path::PathBuf {
        let mut s = path.as_os_str().to_owned();
        s.push(format!(".bak.{label}"));
        std::path::PathBuf::from(s)
    }

    fn tmp_path(path: &Path, label: &str) -> std::path::PathBuf {
        let mut s = path.as_os_str().to_owned();
        s.push(format!(".tmp.{label}"));
        std::path::PathBuf::from(s)
    }

    /// Test-only dotted-path lookup (`serde_yaml::Value` has no `.pointer()`).
    fn at<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
        let mut cur = root;
        for seg in path.split('/').filter(|s| !s.is_empty()) {
            cur = cur.get(seg)?;
        }
        Some(cur)
    }

    fn mcp_block(command: &str) -> Value {
        let mut m = Mapping::new();
        m.insert(Value::String("kind".into()), Value::String("mcp".into()));
        m.insert(
            Value::String("command".into()),
            Value::String(command.into()),
        );
        Value::Mapping(m)
    }

    const BASE_FIXTURE: &str = r#"
version: "1.0.0"
store:
  kind: file
  path: ./state
connections:
  github:
    kind: mcp
    command: github-mcp-server
  dotnet:
    kind: cli
    command: dotnet
"#;

    #[test]
    fn add_inserts_under_connections_and_backs_up_original() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(dir.path(), "gateway.yaml", BASE_FIXTURE);

        add_connection(&path, "newtool", &mcp_block("newtool-mcp-server"), "t1").unwrap();

        let root: Value = serde_yaml::from_str(&read_str(&path)).unwrap();
        let block = at(&root, "connections/newtool").unwrap();
        assert_eq!(
            block.get("command").and_then(Value::as_str),
            Some("newtool-mcp-server")
        );
        assert_eq!(block.get("kind").and_then(Value::as_str), Some("mcp"));

        // backup exists and holds the ORIGINAL content.
        let bak = backup_path(&path, "t1");
        assert!(bak.exists(), "backup file must exist");
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), BASE_FIXTURE);
    }

    #[test]
    fn add_existing_name_errors_and_leaves_file_and_backup_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(dir.path(), "gateway.yaml", BASE_FIXTURE);

        let err = add_connection(&path, "github", &mcp_block("evil-override"), "t2").unwrap_err();
        assert!(matches!(err, ConfigMutationError::NameExists(n) if n == "github"));

        // original untouched
        assert_eq!(read_str(&path), BASE_FIXTURE);
        // no backup was created — nothing was written
        assert!(!backup_path(&path, "t2").exists());
    }

    #[test]
    fn add_preserves_other_connections_and_unrelated_top_level_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(dir.path(), "gateway.yaml", BASE_FIXTURE);

        add_connection(&path, "newtool", &mcp_block("newtool-mcp-server"), "t3").unwrap();

        let root: Value = serde_yaml::from_str(&read_str(&path)).unwrap();
        assert_eq!(at(&root, "version").and_then(Value::as_str), Some("1.0.0"));
        assert_eq!(
            at(&root, "store/kind").and_then(Value::as_str),
            Some("file")
        );
        assert_eq!(
            at(&root, "connections/github/command").and_then(Value::as_str),
            Some("github-mcp-server")
        );
        assert_eq!(
            at(&root, "connections/dotnet/command").and_then(Value::as_str),
            Some("dotnet")
        );
        // 3 connections now: github, dotnet, newtool
        let conns = at(&root, "connections").unwrap().as_mapping().unwrap();
        assert_eq!(conns.len(), 3);
    }

    #[test]
    fn remove_deletes_named_connection_and_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(dir.path(), "gateway.yaml", BASE_FIXTURE);

        remove_connection(&path, "dotnet", "t4").unwrap();

        let root: Value = serde_yaml::from_str(&read_str(&path)).unwrap();
        let conns = at(&root, "connections").unwrap().as_mapping().unwrap();
        assert_eq!(conns.len(), 1);
        assert!(at(&root, "connections/dotnet").is_none());
        assert!(at(&root, "connections/github").is_some());

        let bak = backup_path(&path, "t4");
        assert!(bak.exists());
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), BASE_FIXTURE);
    }

    #[test]
    fn remove_absent_connection_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(dir.path(), "gateway.yaml", BASE_FIXTURE);

        remove_connection(&path, "does-not-exist", "t5").unwrap();

        // nothing changed, no backup, no tmp
        assert_eq!(read_str(&path), BASE_FIXTURE);
        assert!(!backup_path(&path, "t5").exists());
        assert!(!tmp_path(&path, "t5").exists());
    }

    #[test]
    fn successful_op_leaves_no_leftover_tmp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(dir.path(), "gateway.yaml", BASE_FIXTURE);

        add_connection(&path, "newtool", &mcp_block("x"), "t6").unwrap();
        assert!(!tmp_path(&path, "t6").exists());

        remove_connection(&path, "newtool", "t7").unwrap();
        assert!(!tmp_path(&path, "t7").exists());

        // sanity: no stray .tmp.* files at all in the dir
        let leftover: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftover.is_empty(), "leftover tmp files: {leftover:?}");
    }

    #[test]
    fn malformed_yaml_errors_parse_and_leaves_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let malformed = "connections:\n  github:\n  - not: [a, mapping\n";
        let path = write_fixture(dir.path(), "gateway.yaml", malformed);

        let err = add_connection(&path, "x", &mcp_block("x"), "t8").unwrap_err();
        assert!(matches!(err, ConfigMutationError::Parse(_)));

        assert_eq!(
            read_str(&path),
            malformed,
            "original must be byte-identical"
        );
        assert!(!backup_path(&path, "t8").exists());
    }

    #[test]
    fn add_creates_connections_mapping_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let no_conns = "version: \"1.0.0\"\nstore:\n  kind: memory\n";
        let path = write_fixture(dir.path(), "gateway.yaml", no_conns);

        add_connection(&path, "solo", &mcp_block("solo-mcp"), "t9").unwrap();

        let root: Value = serde_yaml::from_str(&read_str(&path)).unwrap();
        assert_eq!(
            at(&root, "connections/solo/command").and_then(Value::as_str),
            Some("solo-mcp")
        );
        assert_eq!(
            at(&root, "store/kind").and_then(Value::as_str),
            Some("memory")
        );
    }

    #[test]
    fn remove_on_file_with_no_connections_key_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let no_conns = "version: \"1.0.0\"\n";
        let path = write_fixture(dir.path(), "gateway.yaml", no_conns);

        remove_connection(&path, "whatever", "t10").unwrap();
        assert_eq!(read_str(&path), no_conns);
        assert!(!backup_path(&path, "t10").exists());
    }
}
