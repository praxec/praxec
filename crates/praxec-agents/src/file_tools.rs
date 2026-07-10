//! A **scoped file-edit `ToolHost`** — gives a *trusted* (in-process) rig agent
//! `write_file` / `read_file` tools rooted at a directory. The root travels as
//! the `connection` string (the [`ToolHost`] contract is stateless), so one
//! shared host serves concurrent agents, each scoped to its own root. Every
//! path is resolved under that root; a path that is absolute or escapes the
//! root via `..` is refused (no traversal) — fail-fast, never a silent write
//! outside scope.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::error::ExecutorError;
use rig::completion::ToolDefinition;
use serde_json::{Value, json};

use crate::rig_runner::ToolHost;

/// Maximum lines an existing file may have for `write_file` to overwrite it.
/// write_file truncates the entire file before writing, so a context-limited
/// agent calling it on a large existing file (e.g. 3000+ lines) can corrupt
/// it.  Above this threshold, the tool REFUSES and directs the agent to use
/// `edit_file` (surgical old_string→new_string) instead.
const WRITE_FILE_MAX_OVERWRITE_LINES: usize = 300;

/// A stateless file-edit tool host. The repo root is supplied per-call as the
/// `connection` string, so the host holds no per-session state.
#[derive(Default)]
pub struct FileEditToolHost;

impl FileEditToolHost {
    pub fn new() -> Self {
        Self
    }
}

/// Resolve `rel` under `root`, refusing absolute paths and any `..` escape.
fn resolve_under(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let p = Path::new(rel);
    if p.is_absolute() {
        return Err(format!(
            "FILE_TOOL_PATH_ESCAPE: '{rel}' is absolute; paths must be relative to the repo root"
        ));
    }
    if p.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(format!(
            "FILE_TOOL_PATH_ESCAPE: '{rel}' contains '..'; paths may not escape the repo root"
        ));
    }
    Ok(root.join(p))
}

fn write_file_def() -> ToolDefinition {
    ToolDefinition {
        name: "write_file".into(),
        description: "Create or overwrite a file at `path` (relative to the repo root) with \
                      `content`. Missing parent directories are created."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "repo-root-relative path" },
                "content": { "type": "string" }
            },
            "required": ["path", "content"]
        }),
    }
}

fn read_file_def() -> ToolDefinition {
    ToolDefinition {
        name: "read_file".into(),
        description: "Read the file at `path` (relative to the repo root); returns its contents."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "repo-root-relative path" }
            },
            "required": ["path"]
        }),
    }
}

fn edit_file_def() -> ToolDefinition {
    ToolDefinition {
        name: "edit_file".into(),
        description: "Replace exactly ONE unique occurrence of `old_string` with `new_string` in \
                      the file at `path` (relative to repo root). Preferred over write_file for \
                      editing existing files — it never overwrites the whole file. `old_string` \
                      must match exactly once (include surrounding context to disambiguate)."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "repo-root-relative path" },
                "old_string": { "type": "string" },
                "new_string": { "type": "string" }
            },
            "required": ["path", "old_string", "new_string"]
        }),
    }
}

fn search_file_def() -> ToolDefinition {
    ToolDefinition {
        name: "search_file".into(),
        description: "Return lines (with 1-based line numbers) in `path` containing `pattern`. \
                      Use this to locate an edit anchor in a large file without reading it whole."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "repo-root-relative path" },
                "pattern": { "type": "string" }
            },
            "required": ["path", "pattern"]
        }),
    }
}

fn read_range_def() -> ToolDefinition {
    ToolDefinition {
        name: "read_range".into(),
        description: "Read lines `start`..`end` (1-based, inclusive) of `path` — a bounded window \
                      into a large file."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "repo-root-relative path" },
                "start": { "type": "integer" },
                "end": { "type": "integer" }
            },
            "required": ["path", "start", "end"]
        }),
    }
}

#[async_trait]
impl ToolHost for FileEditToolHost {
    async fn tools(
        &self,
        connections: &[String],
    ) -> Result<Vec<(ToolDefinition, String)>, ExecutorError> {
        let mut out = Vec::with_capacity(connections.len() * 2);
        for conn in connections {
            out.push((write_file_def(), conn.clone()));
            out.push((read_file_def(), conn.clone()));
            out.push((edit_file_def(), conn.clone()));
            out.push((search_file_def(), conn.clone()));
            out.push((read_range_def(), conn.clone()));
        }
        Ok(out)
    }

    async fn call(&self, connection: &str, name: &str, arguments: &str) -> Result<String, String> {
        let root = Path::new(connection);
        let args: Value = serde_json::from_str(arguments)
            .map_err(|e| format!("FILE_TOOL_BAD_ARGS: invalid JSON: {e}"))?;
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| "FILE_TOOL_BAD_ARGS: missing string 'path'".to_string())?;
        let target = resolve_under(root, path)?;
        match name {
            "write_file" => {
                let content = args
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "FILE_TOOL_BAD_ARGS: missing string 'content'".to_string())?;
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("FILE_TOOL_WRITE_FAILED: {e}"))?;
                }
                // Guard: refuse to overwrite a large existing file — a
                // context-limited agent calling write_file on a big file
                // truncates it.  Above the threshold, direct to edit_file.
                if target.exists() {
                    let existing = std::fs::read_to_string(&target).unwrap_or_default();
                    let n_lines = existing.lines().count();
                    if n_lines > WRITE_FILE_MAX_OVERWRITE_LINES {
                        return Err(format!(
                            "FILE_TOOL_REFUSED_OVERWRITE: '{path}' already has {n_lines} lines; \
                             write_file would overwrite the whole file and risks truncation. \
                             Use edit_file (surgical old_string->new_string) for changes to \
                             large files."
                        ));
                    }
                }
                std::fs::write(&target, content)
                    .map_err(|e| format!("FILE_TOOL_WRITE_FAILED: {e}"))?;
                Ok(format!("wrote {path} ({} bytes)", content.len()))
            }
            "read_file" => {
                std::fs::read_to_string(&target).map_err(|e| format!("FILE_TOOL_READ_FAILED: {e}"))
            }
            "edit_file" => {
                // Surgical edit: replace exactly ONE unique occurrence of
                // `old_string` with `new_string`. Unlike write_file this never
                // overwrites the whole file, so a context-limited agent editing a
                // large file can't truncate it. Fail-fast on absent/ambiguous.
                let old = args
                    .get("old_string")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "FILE_TOOL_BAD_ARGS: missing string 'old_string'".to_string())?;
                let new = args
                    .get("new_string")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "FILE_TOOL_BAD_ARGS: missing string 'new_string'".to_string())?;
                let content = std::fs::read_to_string(&target)
                    .map_err(|e| format!("FILE_TOOL_READ_FAILED: {e}"))?;
                match content.matches(old).count() {
                    0 => Err(format!("EDIT_NO_MATCH: 'old_string' not found in {path}")),
                    1 => {
                        let updated = content.replacen(old, new, 1);
                        std::fs::write(&target, updated)
                            .map_err(|e| format!("FILE_TOOL_WRITE_FAILED: {e}"))?;
                        Ok(format!("edited {path}"))
                    }
                    n => Err(format!(
                        "EDIT_AMBIGUOUS: 'old_string' occurs {n}x in {path}; add surrounding \
                         context so it matches exactly once"
                    )),
                }
            }
            "search_file" => {
                // Find matching lines (with 1-based numbers) so the agent can
                // locate an edit anchor in a large file WITHOUT reading the whole
                // file — the key to self-editing big files within one turn.
                let pattern = args
                    .get("pattern")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "FILE_TOOL_BAD_ARGS: missing string 'pattern'".to_string())?;
                let content = std::fs::read_to_string(&target)
                    .map_err(|e| format!("FILE_TOOL_READ_FAILED: {e}"))?;
                let hits: Vec<String> = content
                    .lines()
                    .enumerate()
                    .filter(|(_, l)| l.contains(pattern))
                    .map(|(i, l)| format!("{}: {l}", i + 1))
                    .collect();
                if hits.is_empty() {
                    Ok(format!("(no lines match {pattern:?} in {path})"))
                } else {
                    Ok(hits.join("\n"))
                }
            }
            "read_range" => {
                // Read lines [start,end] (1-based, inclusive) — a bounded window
                // into a large file.
                let start = args.get("start").and_then(Value::as_u64).ok_or_else(|| {
                    "FILE_TOOL_BAD_ARGS: missing integer 'start' (1-based)".to_string()
                })? as usize;
                let end = args
                    .get("end")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| "FILE_TOOL_BAD_ARGS: missing integer 'end'".to_string())?
                    as usize;
                if start == 0 || end < start {
                    return Err(format!(
                        "FILE_TOOL_BAD_RANGE: start={start} end={end} (1-based, start<=end)"
                    ));
                }
                let content = std::fs::read_to_string(&target)
                    .map_err(|e| format!("FILE_TOOL_READ_FAILED: {e}"))?;
                let lines: Vec<&str> = content.lines().collect();
                if start > lines.len() {
                    return Err(format!(
                        "FILE_TOOL_BAD_RANGE: start {start} beyond EOF ({} lines)",
                        lines.len()
                    ));
                }
                let e = end.min(lines.len());
                Ok(lines[start - 1..e]
                    .iter()
                    .enumerate()
                    .map(|(i, l)| format!("{}: {l}", start + i))
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            other => Err(format!(
                "FILE_TOOL_UNKNOWN: '{other}' (expected write_file|read_file|edit_file|search_file|read_range)"
            )),
        }
    }
}

/// Connection-name prefix that routes a tool call to the scoped file host; the
/// remainder is the repo root. e.g. `file:/home/me/markdown-mcp`.
pub const FILE_CONNECTION_PREFIX: &str = "file:";

/// Routes tool calls by connection: `file:<root>` connections go to a
/// [`FileEditToolHost`] (scoped at `<root>`), everything else to the wrapped MCP
/// host. Lets one runner serve both the engine MCP tools and scoped file-edit
/// tools — the in-process trusted coding agent's toolbelt.
pub struct CompositeToolHost {
    mcp: Arc<dyn ToolHost>,
    files: FileEditToolHost,
}

impl CompositeToolHost {
    pub fn new(mcp: Arc<dyn ToolHost>) -> Self {
        Self {
            mcp,
            files: FileEditToolHost::new(),
        }
    }
}

#[async_trait]
impl ToolHost for CompositeToolHost {
    async fn tools(
        &self,
        connections: &[String],
    ) -> Result<Vec<(ToolDefinition, String)>, ExecutorError> {
        let mut mcp_conns: Vec<String> = Vec::new();
        let mut file_conns: Vec<(String, String)> = Vec::new();
        for c in connections {
            match c.strip_prefix(FILE_CONNECTION_PREFIX) {
                // Only an ABSOLUTE root is a real repo. An empty/unresolved
                // (`{{…}}`) or relative root — e.g. a non-coding leaf where
                // `repo_path` didn't resolve — exposes NO file tools, so a
                // relative path can never fall back to the server's cwd.
                Some(root) if Path::new(root).is_absolute() => {
                    file_conns.push((c.clone(), root.to_string()))
                }
                Some(_) => {}
                None => mcp_conns.push(c.clone()),
            }
        }
        let mut out = if mcp_conns.is_empty() {
            Vec::new()
        } else {
            self.mcp.tools(&mcp_conns).await?
        };
        for (orig, root) in file_conns {
            for (def, _root) in self.files.tools(&[root]).await? {
                out.push((def, orig.clone()));
            }
        }
        Ok(out)
    }

    async fn call(&self, connection: &str, name: &str, arguments: &str) -> Result<String, String> {
        match connection.strip_prefix(FILE_CONNECTION_PREFIX) {
            Some(root) if Path::new(root).is_absolute() => {
                self.files.call(root, name, arguments).await
            }
            Some(root) => Err(format!(
                "FILE_TOOL_ROOT_INVALID: '{root}' is not an absolute repo root (unresolved or \
                 relative); refusing to write"
            )),
            None => self.mcp.call(connection, name, arguments).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::rig_runner::ToolHost;
    use serde_json::json;

    #[tokio::test]
    async fn write_file_creates_file_under_root() {
        let root = tempfile::tempdir().unwrap();
        let host = super::FileEditToolHost::new();
        host.call(
            root.path().to_str().unwrap(),
            "write_file",
            &json!({ "path": "src/lib.rs", "content": "// hi\n" }).to_string(),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join("src/lib.rs")).unwrap(),
            "// hi\n"
        );
    }

    #[tokio::test]
    async fn write_file_to_new_path_succeeds() {
        let root = tempfile::tempdir().unwrap();
        let host = super::FileEditToolHost::new();
        let out = host
            .call(
                root.path().to_str().unwrap(),
                "write_file",
                &json!({ "path": "fresh/new_file.rs", "content": "// brand-new\n" }).to_string(),
            )
            .await
            .unwrap();
        assert!(out.contains("fresh/new_file.rs"), "{out}");
        assert_eq!(
            std::fs::read_to_string(root.path().join("fresh/new_file.rs")).unwrap(),
            "// brand-new\n"
        );
    }

    #[tokio::test]
    async fn write_file_overwrites_small_existing_file() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("small.rs"), "line1\nline2\nline3\n").unwrap();
        let host = super::FileEditToolHost::new();
        let out = host
            .call(
                root.path().to_str().unwrap(),
                "write_file",
                &json!({ "path": "small.rs", "content": "replaced\n" }).to_string(),
            )
            .await
            .unwrap();
        assert!(out.contains("small.rs"), "{out}");
        assert_eq!(
            std::fs::read_to_string(root.path().join("small.rs")).unwrap(),
            "replaced\n"
        );
    }

    #[tokio::test]
    async fn write_file_refuses_large_existing_file_and_preserves_content() {
        let root = tempfile::tempdir().unwrap();
        // 301 lines — just above the threshold
        let original = (1..=301)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(root.path().join("big.rs"), &original).unwrap();
        let host = super::FileEditToolHost::new();
        let err = host
            .call(
                root.path().to_str().unwrap(),
                "write_file",
                &json!({ "path": "big.rs", "content": "SHOULD NOT BE WRITTEN\n" }).to_string(),
            )
            .await
            .unwrap_err();
        assert!(
            err.contains("FILE_TOOL_REFUSED_OVERWRITE"),
            "expected FILE_TOOL_REFUSED_OVERWRITE: {err}"
        );
        assert!(err.contains("301"), "expected line count in error: {err}");
        // File must be UNCHANGED
        assert_eq!(
            std::fs::read_to_string(root.path().join("big.rs")).unwrap(),
            original,
            "large file must be untouched"
        );
    }

    #[tokio::test]
    async fn read_file_returns_content_under_root() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.txt"), "alpha").unwrap();
        let host = super::FileEditToolHost::new();
        let out = host
            .call(
                root.path().to_str().unwrap(),
                "read_file",
                &json!({ "path": "a.txt" }).to_string(),
            )
            .await
            .unwrap();
        assert!(out.contains("alpha"), "{out}");
    }

    #[tokio::test]
    async fn edit_file_replaces_a_unique_span_without_overwriting() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("lib.rs"),
            "fn a() {}\nfn b() {}\nfn c() {}\n",
        )
        .unwrap();
        let host = super::FileEditToolHost::new();
        host.call(
            root.path().to_str().unwrap(),
            "edit_file",
            &json!({ "path": "lib.rs", "old_string": "fn b() {}", "new_string": "fn b() { 0 }" })
                .to_string(),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join("lib.rs")).unwrap(),
            "fn a() {}\nfn b() { 0 }\nfn c() {}\n",
            "only the unique span changes; the rest is preserved"
        );
    }

    #[tokio::test]
    async fn search_file_returns_matching_lines_with_numbers() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("g.rs"),
            "alpha\nenum Command {\n  Check,\n}\nbeta\n",
        )
        .unwrap();
        let host = super::FileEditToolHost::new();
        let out = host
            .call(
                root.path().to_str().unwrap(),
                "search_file",
                &json!({ "path": "g.rs", "pattern": "Command" }).to_string(),
            )
            .await
            .unwrap();
        assert!(out.contains("2:"), "reports the line number: {out}");
        assert!(out.contains("enum Command"), "{out}");
        assert!(!out.contains("alpha"), "only matching lines: {out}");
    }

    #[tokio::test]
    async fn read_range_returns_only_the_requested_lines() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("g.rs"), "l1\nl2\nl3\nl4\nl5\n").unwrap();
        let host = super::FileEditToolHost::new();
        let out = host
            .call(
                root.path().to_str().unwrap(),
                "read_range",
                &json!({ "path": "g.rs", "start": 2, "end": 4 }).to_string(),
            )
            .await
            .unwrap();
        assert!(
            out.contains("l2") && out.contains("l3") && out.contains("l4"),
            "{out}"
        );
        assert!(
            !out.contains("l1") && !out.contains("l5"),
            "range only: {out}"
        );
    }

    #[tokio::test]
    async fn edit_file_refuses_absent_or_ambiguous_old_string() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("f.rs"), "x\nx\n").unwrap();
        let host = super::FileEditToolHost::new();
        let e1 = host
            .call(
                root.path().to_str().unwrap(),
                "edit_file",
                &json!({ "path": "f.rs", "old_string": "zzz", "new_string": "y" }).to_string(),
            )
            .await
            .unwrap_err();
        assert!(e1.contains("EDIT_NO_MATCH"), "{e1}");
        let e2 = host
            .call(
                root.path().to_str().unwrap(),
                "edit_file",
                &json!({ "path": "f.rs", "old_string": "x", "new_string": "y" }).to_string(),
            )
            .await
            .unwrap_err();
        assert!(e2.contains("EDIT_AMBIGUOUS"), "{e2}");
        assert_eq!(
            std::fs::read_to_string(root.path().join("f.rs")).unwrap(),
            "x\nx\n",
            "file untouched on any edit error"
        );
    }

    #[tokio::test]
    async fn path_escaping_root_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let host = super::FileEditToolHost::new();
        let err = host
            .call(
                root.path().to_str().unwrap(),
                "write_file",
                &json!({ "path": "../escape.rs", "content": "x" }).to_string(),
            )
            .await
            .unwrap_err();
        assert!(err.contains("PATH_ESCAPE"), "{err}");
        assert!(
            !root.path().parent().unwrap().join("escape.rs").exists(),
            "nothing written outside the root"
        );
    }

    #[tokio::test]
    async fn tools_lists_write_and_read_paired_with_the_root() {
        let host = super::FileEditToolHost::new();
        let tools = host.tools(&["/some/root".to_string()]).await.unwrap();
        let names: Vec<&str> = tools.iter().map(|(d, _c)| d.name.as_str()).collect();
        assert!(names.contains(&"write_file"), "{names:?}");
        assert!(names.contains(&"read_file"), "{names:?}");
        assert!(
            tools.iter().all(|(_d, c)| c == "/some/root"),
            "each tool is routed to the root connection"
        );
    }

    use praxec_core::error::ExecutorError;
    use rig::completion::ToolDefinition;

    struct StubMcp;
    #[async_trait::async_trait]
    impl ToolHost for StubMcp {
        async fn tools(
            &self,
            conns: &[String],
        ) -> Result<Vec<(ToolDefinition, String)>, ExecutorError> {
            Ok(conns
                .iter()
                .map(|c| {
                    (
                        ToolDefinition {
                            name: "mcp_tool".into(),
                            description: String::new(),
                            parameters: json!({ "type": "object" }),
                        },
                        c.clone(),
                    )
                })
                .collect())
        }
        async fn call(&self, connection: &str, name: &str, _a: &str) -> Result<String, String> {
            Ok(format!("mcp:{connection}:{name}"))
        }
    }

    #[tokio::test]
    async fn composite_routes_file_connections_to_files_and_rest_to_mcp() {
        let root = tempfile::tempdir().unwrap();
        let comp = super::CompositeToolHost::new(std::sync::Arc::new(StubMcp));
        let fileconn = format!("file:{}", root.path().to_str().unwrap());

        let tools = comp
            .tools(&[fileconn.clone(), "engine".to_string()])
            .await
            .unwrap();
        let names: Vec<&str> = tools.iter().map(|(d, _)| d.name.as_str()).collect();
        assert!(
            names.contains(&"write_file")
                && names.contains(&"read_file")
                && names.contains(&"mcp_tool"),
            "{names:?}"
        );

        comp.call(
            &fileconn,
            "write_file",
            &json!({ "path": "x.rs", "content": "hi" }).to_string(),
        )
        .await
        .unwrap();
        assert!(root.path().join("x.rs").exists(), "file write routed to fs");

        let out = comp.call("engine", "do", "{}").await.unwrap();
        assert!(out.starts_with("mcp:engine"), "mcp call routed: {out}");
    }

    #[tokio::test]
    async fn unresolved_file_root_yields_no_tools_and_refuses_writes() {
        // A non-coding leaf renders `file:{{repo_path}}` to an empty/stub root.
        // Such a connection must expose NO file tools, and a write must be
        // refused — never fall back to a relative root (which would be the
        // server's cwd, i.e. the live repo).
        let comp = super::CompositeToolHost::new(std::sync::Arc::new(StubMcp));
        for bad in [
            "file:",
            "file:{{ $.workflow.input.repo_path }}",
            "file:rel/path",
        ] {
            let tools = comp.tools(&[bad.to_string()]).await.unwrap();
            assert!(
                tools.is_empty(),
                "unresolved/relative root must expose no tools: {bad} -> {tools:?}"
            );
            let err = comp
                .call(
                    bad,
                    "write_file",
                    &json!({ "path": "x", "content": "y" }).to_string(),
                )
                .await
                .unwrap_err();
            assert!(err.contains("FILE_TOOL_ROOT"), "{err}");
        }
    }
}
