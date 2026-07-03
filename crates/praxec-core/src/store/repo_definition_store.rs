//! Repo-backed [`DefinitionStoreWritable`] — the missing write **keystone**
//! (SPEC §8.4). The trait, the `registry` executor, and `write_enabled` all
//! existed; nothing actually wrote a definition to disk. This does.
//!
//! **Multi-repo aware** (SPEC §9): definitions are namespace-prefixed
//! (`cognitive/flow.x`, `acme/cap.y`), and the store routes each id to its
//! owning repo. You *consume* third-party repos (e.g. `cognitive-architectures`)
//! **read-only**, and *author* into repos explicitly marked writable (your own /
//! team / company). On-disk shape mirrors the repos: one definition per file,
//! `<repo>/<layout-tier>/<local-id>.yaml` holding `workflows: { <local-id>: … }`.
//!
//! Per SPEC §8.4, `register` is **audit-before-commit**: it emits
//! `definition.published` to the audit sink (failure → `RECORD_WRITE_FAILED`,
//! abort with no write), then writes the file, then **git commits** in that repo.
//! The commit shells out to `git` (matching the codebase's existing git use, and
//! reusing the operator's git auth for the eventual push) — gix's
//! commit-from-worktree path is lower-level than libgit2; gix is reserved for the
//! read/browse side.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::audit::{AuditEvent, AuditSink};
use crate::ports::{DefinitionStore, DefinitionStoreWritable};
use crate::repo::{RepoLayout, load_manifest};

/// One configured resource repo the store can route to.
pub struct RepoEntry {
    pub namespace: String,
    pub root: PathBuf,
    pub layout: RepoLayout,
    /// Authoring writes are allowed only into repos explicitly marked writable.
    pub writable: bool,
    /// Publish authored commits to the repo's remote (`git push`) after each
    /// successful register. Off by default — local commit only.
    pub push: bool,
}

/// Routes definition reads/writes across the configured (layered) repos.
pub struct RepoDefinitionStore {
    by_namespace: HashMap<String, RepoEntry>,
    audit: Arc<dyn AuditSink>,
}

impl RepoDefinitionStore {
    /// Build from the configured repos (`(path, writable, push)`) + an audit
    /// sink, loading each manifest for its namespace + layout. On a namespace
    /// clash the later repo wins (mirrors config merge order).
    pub fn from_repos(
        repos: impl IntoIterator<Item = (PathBuf, bool, bool)>,
        audit: Arc<dyn AuditSink>,
    ) -> anyhow::Result<Self> {
        let mut by_namespace = HashMap::new();
        for (root, writable, push) in repos {
            let manifest = load_manifest(&root)?;
            by_namespace.insert(
                manifest.namespace.clone(),
                RepoEntry {
                    namespace: manifest.namespace,
                    root,
                    layout: manifest.layout,
                    writable,
                    push,
                },
            );
        }
        Ok(Self {
            by_namespace,
            audit,
        })
    }

    fn split(id: &str) -> anyhow::Result<(&str, &str)> {
        id.split_once('/').ok_or_else(|| {
            anyhow::anyhow!("definition id '{id}' is not namespace-prefixed (`<namespace>/<id>`)")
        })
    }

    /// The layout directory for a local id, by the repo tier convention
    /// (`cap.*`→capabilities, `flow.*`→flows, `skill.*`→skills,
    /// `script.*`→scripts; anything else → flows).
    fn tier_dir<'a>(layout: &'a RepoLayout, local_id: &str) -> &'a str {
        if local_id.starts_with("cap.") {
            &layout.capabilities
        } else if local_id.starts_with("skill.") {
            &layout.skills
        } else if local_id.starts_with("script.") {
            &layout.scripts
        } else {
            &layout.flows
        }
    }

    /// The repo entry owning `namespaced_id`, requiring it be writable.
    fn writable_entry(&self, namespaced_id: &str) -> anyhow::Result<(&RepoEntry, String)> {
        let (ns, local) = Self::split(namespaced_id)?;
        let entry = self.by_namespace.get(ns).ok_or_else(|| {
            anyhow::anyhow!("no configured repo for namespace '{ns}' (id '{namespaced_id}')")
        })?;
        if !entry.writable {
            anyhow::bail!("repo '{ns}' is read-only — author into a writable repo");
        }
        Ok((entry, local.to_string()))
    }

    /// The on-disk file for a local id in a repo (one definition per file).
    fn file_path(entry: &RepoEntry, local_id: &str) -> PathBuf {
        entry
            .root
            .join(Self::tier_dir(&entry.layout, local_id))
            .join(format!("{local_id}.yaml"))
    }
}

#[async_trait]
impl DefinitionStore for RepoDefinitionStore {
    async fn load(&self, definition_id: &str) -> anyhow::Result<Value> {
        let (ns, local) = Self::split(definition_id)?;
        let entry = self
            .by_namespace
            .get(ns)
            .ok_or_else(|| anyhow::anyhow!("no configured repo for namespace '{ns}'"))?;
        let path = Self::file_path(entry, local);
        let text = std::fs::read_to_string(&path).map_err(|e| {
            anyhow::anyhow!(
                "reading definition '{definition_id}' at {}: {e}",
                path.display()
            )
        })?;
        let file: Value = serde_yaml::from_str(&text)?;
        file.get("workflows")
            .and_then(|w| w.get(local))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("definition '{local}' not found in {}", path.display()))
    }
}

#[async_trait]
impl DefinitionStoreWritable for RepoDefinitionStore {
    async fn register(
        &self,
        definition_id: &str,
        definition: Value,
        expected_prior_hash: Option<&str>,
    ) -> anyhow::Result<()> {
        let (entry, local) = self.writable_entry(definition_id)?;
        let path = Self::file_path(entry, &local);

        // SPEC §8.4 optimistic concurrency: the snapshot the author edited must
        // still be the one on disk. We hash the CURRENT on-disk definition (if
        // any) and reject CONFLICT_STALE on mismatch — before any write, audit,
        // or commit. `None` skips the check (a fresh create-or-overwrite).
        if let Some(expected) = expected_prior_hash {
            // Distinguish "no prior definition on disk" (a legitimate fresh
            // create — None) from a genuine read/parse failure. Masking an I/O
            // or corrupt-YAML error as `None` would mislabel it as
            // CONFLICT_STALE and send the operator to re-read a definition that
            // is in fact unreadable. Only a genuinely-absent file is None; any
            // other failure surfaces.
            let current = match self.load(definition_id).await {
                Ok(v) => Some(v),
                Err(e) if !path.exists() => {
                    let _ = e; // file absent → fresh create-or-overwrite
                    None
                }
                Err(e) => {
                    return Err(e.context(format!(
                        "reading current definition '{definition_id}' for the \
                         optimistic-concurrency check"
                    )));
                }
            };
            let current_hash = current.as_ref().map(crate::config::compute_definition_hash);
            if current_hash.as_deref() != Some(expected) {
                anyhow::bail!(
                    "CONFLICT_STALE: '{definition_id}' has changed since you read it \
                     (expected {expected}, found {}). Re-read the current definition and \
                     re-apply your edit.",
                    current_hash.as_deref().unwrap_or("<absent>")
                );
            }
        }

        // SPEC §8.4: audit-before-commit. The published record lands BEFORE the
        // file is written; an audit failure aborts with RECORD_WRITE_FAILED so a
        // definition never becomes loadable without its provenance record.
        let event = AuditEvent::new("definition.published")
            .with_actor("authoring")
            .with_payload(json!({
                "definitionId": definition_id,
                "namespace": entry.namespace,
                "repo": entry.root.display().to_string(),
            }));
        self.audit.record(event).await.map_err(|e| {
            anyhow::anyhow!(
                "RECORD_WRITE_FAILED: audit of '{definition_id}' failed, write aborted: {e}"
            )
        })?;

        // The on-disk shape: a `workflows:` block keyed by the local id.
        let file = json!({ "workflows": { local.clone(): definition } });
        let yaml = serde_yaml::to_string(&file)?;
        write_atomic(&path, &yaml)?;

        // Version it: commit the change in the owning repo.
        git_commit(
            &entry.root,
            &path,
            &format!("author: publish {definition_id}"),
        )?;

        // SPEC §9 — publish to the remote when the repo opted in (`push: true`).
        // Inherits the operator's git auth (no credentials handled here); a push
        // failure is surfaced (REPO_PUSH_FAILED), not swallowed.
        if entry.push {
            crate::repo_git::push(&entry.root)?;
        }
        Ok(())
    }
}

/// Write `contents` to `path` atomically (temp file + rename), creating parents.
fn write_atomic(path: &Path, contents: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("yaml.tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// `git add <file> && git commit -m <msg>` in `repo_root`. Inline identity so the
/// commit doesn't depend on a global git config; `git` must be on PATH and the
/// root must be a git repo (fail-loud otherwise).
fn git_commit(repo_root: &Path, file: &Path, message: &str) -> anyhow::Result<()> {
    let run = |args: &[&str]| -> anyhow::Result<()> {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo_root)
            .args(args)
            .output()
            .map_err(|e| {
                anyhow::anyhow!("running `git {}`: {e} (is git on PATH?)", args.join(" "))
            })?;
        if !out.status.success() {
            anyhow::bail!(
                "`git {}` failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(())
    };
    let rel = file.strip_prefix(repo_root).unwrap_or(file);
    run(&["add", &rel.display().to_string()])?;
    run(&[
        "-c",
        "user.email=authoring@praxec.local",
        "-c",
        "user.name=praxec authoring",
        "commit",
        "-m",
        message,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::NullAuditSink;

    /// An audit sink that always fails — exercises the RECORD_WRITE_FAILED path.
    struct FailingAudit;
    #[async_trait]
    impl AuditSink for FailingAudit {
        async fn record(&self, _event: AuditEvent) -> anyhow::Result<()> {
            anyhow::bail!("audit host unreachable")
        }
    }

    /// A throwaway **git** repo with a `praxec.repo.yaml` (namespace `test`).
    fn temp_repo_with(
        writable: bool,
        audit: Arc<dyn AuditSink>,
    ) -> (tempfile::TempDir, RepoDefinitionStore) {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            Command::new("git")
                .arg("init")
                .arg(dir.path())
                .output()
                .unwrap()
                .status
                .success()
        );
        std::fs::write(
            dir.path().join("praxec.repo.yaml"),
            "schema: praxec.repo/v1\nname: t\nnamespace: test\nversion: 0.0.0\n",
        )
        .unwrap();
        let store =
            RepoDefinitionStore::from_repos([(dir.path().to_path_buf(), writable, false)], audit)
                .unwrap();
        (dir, store)
    }

    fn temp_repo(writable: bool) -> (tempfile::TempDir, RepoDefinitionStore) {
        temp_repo_with(writable, Arc::new(NullAuditSink))
    }

    #[tokio::test]
    async fn register_round_trips_and_commits() {
        let (dir, store) = temp_repo(true);
        let def = json!({ "verb": "implement", "initialState": "ready", "states": {} });
        store
            .register("test/cap.foo", def.clone(), None)
            .await
            .unwrap();
        // Landed at the conventional path (cap.* → capabilities/) and loads back.
        assert!(dir.path().join("capabilities/cap.foo.yaml").exists());
        assert_eq!(store.load("test/cap.foo").await.unwrap(), def);
        // And it was committed to the repo.
        let log = Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["log", "--oneline"])
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&log.stdout).contains("publish test/cap.foo"));
    }

    #[tokio::test]
    async fn flow_ids_route_to_flows() {
        let (dir, store) = temp_repo(true);
        store
            .register("test/flow.bar", json!({ "states": {} }), None)
            .await
            .unwrap();
        assert!(dir.path().join("flows/flow.bar.yaml").exists());
    }

    #[tokio::test]
    async fn audit_failure_aborts_the_write_before_anything_lands() {
        let (dir, store) = temp_repo_with(true, Arc::new(FailingAudit));
        let err = store
            .register("test/cap.x", json!({}), None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("RECORD_WRITE_FAILED"));
        // Audit-before-commit: nothing was written.
        assert!(!dir.path().join("capabilities/cap.x.yaml").exists());
    }

    #[tokio::test]
    async fn read_only_repo_refuses_writes() {
        let (_dir, store) = temp_repo(false);
        let err = store
            .register("test/cap.x", json!({}), None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("read-only"));
    }

    #[tokio::test]
    async fn unknown_namespace_is_rejected() {
        let (_dir, store) = temp_repo(true);
        let err = store
            .register("nope/cap.x", json!({}), None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no configured repo"));
    }

    #[tokio::test]
    async fn unprefixed_id_is_rejected() {
        let (_dir, store) = temp_repo(true);
        assert!(store.register("cap.x", json!({}), None).await.is_err());
    }

    #[tokio::test]
    async fn push_enabled_repo_publishes_commits_to_its_remote() {
        let tmp = tempfile::tempdir().unwrap();
        // A bare origin, and a working clone with a manifest (namespace `test`).
        let origin = tmp.path().join("origin.git");
        assert!(
            Command::new("git")
                .args(["init", "--bare", "-b", "main"])
                .arg(&origin)
                .output()
                .unwrap()
                .status
                .success()
        );
        let work = tmp.path().join("work");
        assert!(
            Command::new("git")
                .args(["clone", &format!("file://{}", origin.display())])
                .arg(&work)
                .output()
                .unwrap()
                .status
                .success()
        );
        std::fs::write(
            work.join("praxec.repo.yaml"),
            "schema: praxec.repo/v1\nname: t\nnamespace: test\nversion: 0.0.0\n",
        )
        .unwrap();
        let git = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(&work)
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        };
        git(&["add", "."]);
        git(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-m",
            "init",
        ]);
        git(&["push", "-u", "origin", "main"]);

        let store = RepoDefinitionStore::from_repos(
            [(work.clone(), true, true)], // writable + push
            Arc::new(NullAuditSink),
        )
        .unwrap();
        store
            .register("test/cap.shipped", json!({ "initialState": "s" }), None)
            .await
            .unwrap();

        // The publish commit reached the bare origin.
        let log = Command::new("git")
            .arg("-C")
            .arg(&origin)
            .args(["log", "--oneline", "main"])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&log.stdout).contains("publish test/cap.shipped"),
            "origin should have the pushed publish commit"
        );
    }

    #[tokio::test]
    async fn edit_with_matching_prior_hash_succeeds_and_stale_hash_conflicts() {
        let (_dir, store) = temp_repo(true);
        let v1 = json!({ "initialState": "a", "states": {} });
        store
            .register("test/cap.edit", v1.clone(), None)
            .await
            .unwrap();

        // Editing on the basis of the current hash succeeds.
        let base = crate::config::compute_definition_hash(&v1);
        let v2 = json!({ "initialState": "b", "states": {} });
        store
            .register("test/cap.edit", v2.clone(), Some(base.as_str()))
            .await
            .unwrap();
        assert_eq!(store.load("test/cap.edit").await.unwrap(), v2);

        // Editing on a STALE basis (the old hash, now superseded) is rejected.
        let err = store
            .register(
                "test/cap.edit",
                json!({ "initialState": "c" }),
                Some(base.as_str()),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("CONFLICT_STALE"), "got: {err}");
        // The conflicting write left the current definition untouched.
        assert_eq!(store.load("test/cap.edit").await.unwrap(), v2);
    }
}
