//! Local `repos` declarations. These edits never resolve remote packs, execute
//! workflows, grant connections, or delete a repository. YAML values and key
//! order survive the round trip; an exact backup preserves comments/formatting.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, bail};
use serde_yaml::{Mapping, Value};

use crate::gateway_config::ReposCommand;

pub(crate) fn run(command: ReposCommand) -> anyhow::Result<()> {
    match command {
        ReposCommand::List { config } => {
            let doc = Document::read(&config)?;
            println!("{}", serde_json::to_string(&doc.entries()?)?);
            Ok(())
        }
        ReposCommand::Add {
            config,
            path,
            writable,
            push,
            stub_manifest,
            name,
        } => add(&config, &path, writable, push, stub_manifest, name),
        ReposCommand::Remove {
            config,
            path_or_name,
        } => remove(&config, &path_or_name),
    }
}

struct Document {
    path: PathBuf,
    original: Vec<u8>,
    value: Value,
}

impl Document {
    fn read(path: &Path) -> anyhow::Result<Self> {
        let path = path
            .canonicalize()
            .context("REPOS_CONFIG: resolving config")?;
        let original = fs::read(&path).context("REPOS_CONFIG: reading config")?;
        let value: Value =
            serde_yaml::from_slice(&original).context("REPOS_CONFIG: parsing YAML")?;
        if !value.is_mapping() {
            bail!("REPOS_CONFIG: gateway config must be a YAML mapping");
        }
        Ok(Self {
            path,
            original,
            value,
        })
    }

    fn entries(&self) -> anyhow::Result<Vec<Value>> {
        match self.value.get("repos") {
            None | Some(Value::Null) => {
                if self.value.get("include").is_some() {
                    bail!(
                        "REPOS_INCLUDED_CONFIG: repos may be inherited; pass --config for the file declaring repos (this command edits only that file)"
                    );
                }
                Ok(Vec::new())
            }
            Some(Value::Sequence(entries)) => Ok(entries.clone()),
            _ => bail!("REPOS_CONFIG: repos must be an array"),
        }
    }

    fn write(&mut self, entries: Vec<Value>) -> anyhow::Result<PathBuf> {
        self.value
            .as_mapping_mut()
            .unwrap()
            .insert(Value::from("repos"), Value::Sequence(entries));
        let bytes = serde_yaml::to_string(&self.value)?.into_bytes();
        let backup = sibling(&self.path, &format!("repos-{}.bak", uuid::Uuid::new_v4()));
        // Refuse observed concurrent edits. The CLI lock coordinates other
        // `repos` processes; this check also catches ordinary editor writes.
        if fs::read(&self.path)? != self.original {
            bail!("REPOS_CONFIG_CHANGED: config changed during edit; retry");
        }
        create_file(&backup, &self.original, &self.path)?;
        if let Err(err) = replace_file(&self.path, &self.original, &bytes) {
            let _ = fs::remove_file(&backup);
            return Err(err);
        }
        Ok(backup)
    }
}

struct EditLock(PathBuf);
impl EditLock {
    fn acquire(config: &Path) -> anyhow::Result<Self> {
        let path = sibling(config, "repos.lock");
        let mut file = OpenOptions::new().write(true).create_new(true).open(&path)
            .with_context(|| format!("REPOS_EDIT_LOCKED: {} (another edit is active; remove a stale lock only after verifying its process is gone)", path.display()))?;
        writeln!(file, "pid={}", std::process::id())?;
        Ok(Self(path))
    }
}
impl Drop for EditLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    path.with_file_name(format!(
        ".{}.{}",
        path.file_name().unwrap().to_string_lossy(),
        suffix
    ))
}

fn create_file(path: &Path, bytes: &[u8], permissions_from: &Path) -> anyhow::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    // Match the original's access controls, including configs holding secrets.
    file.set_permissions(fs::metadata(permissions_from)?.permissions())?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn replace_file(path: &Path, expected: &[u8], bytes: &[u8]) -> anyhow::Result<()> {
    let temporary = sibling(path, &format!("repos-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        create_file(&temporary, bytes, path)?;
        if fs::read(path)? != expected {
            bail!(
                "REPOS_CONFIG_CHANGED: {} changed during edit; retry",
                path.display()
            );
        }
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn normalized(path: &Path) -> anyhow::Result<PathBuf> {
    if let Ok(canonical) = path.canonicalize() {
        return Ok(canonical);
    }
    let absolute = std::path::absolute(path)?;
    let mut result = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    Ok(result)
}

fn declared_path(entry: &Value, config_dir: &Path) -> anyhow::Result<Option<PathBuf>> {
    let Some(path) = entry.get("path").and_then(Value::as_str) else {
        return Ok(None);
    };
    let path = if let Some(tail) = path.strip_prefix("~/") {
        dirs::home_dir()
            .context("REPOS_PATH: cannot resolve ~/ without a home directory")?
            .join(tail)
    } else {
        PathBuf::from(path)
    };
    normalized(&config_dir.join(path)).map(Some)
}

/// Prefer config-relative paths so the edit does not introduce machine paths
/// into the worktree-native contract. A different Windows drive stays absolute.
fn relative_path(target: &Path, base: &Path) -> PathBuf {
    let target_parts: Vec<_> = target.components().collect();
    let base_parts: Vec<_> = base.components().collect();
    let common = target_parts
        .iter()
        .zip(&base_parts)
        .take_while(|(a, b)| a == b)
        .count();
    if common == 0 {
        return target.to_path_buf();
    }
    let mut out = PathBuf::new();
    for _ in common..base_parts.len() {
        out.push("..");
    }
    for component in &target_parts[common..] {
        out.push(component.as_os_str());
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

fn add(
    config: &Path,
    path: &Path,
    writable: bool,
    push: bool,
    stub: bool,
    name: Option<String>,
) -> anyhow::Result<()> {
    let mut doc = Document::read(config)?;
    let _lock = EditLock::acquire(&doc.path)?;
    let mut entries = doc.entries()?;
    let root = path
        .canonicalize()
        .context("REPO_ROOT_MISSING: local root must exist")?;
    if !root.is_dir() {
        bail!("REPO_ROOT_MISSING: local root must be a directory");
    }
    fs::metadata(root.join(".")).context("REPO_ROOT_MISSING: local root must be searchable")?;
    fs::read_dir(&root).context("REPO_ROOT_MISSING: local root must be readable")?;
    if push {
        git_root(&root).context("REPOS_PUSH_REQUIRES_GIT: --push requires a Git worktree root")?;
    }
    if name.as_deref().is_some_and(|n| n.trim().is_empty()) {
        bail!("INVALID_REPO_NAME: name must not be blank");
    }
    for entry in &entries {
        if declared_path(entry, doc.path.parent().unwrap())?.as_ref() == Some(&root)
            || name
                .as_ref()
                .is_some_and(|n| entry.get("name").and_then(Value::as_str) == Some(n.as_str()))
        {
            bail!(
                "DUPLICATE_REPO: path or name is already declared; remove it explicitly before changing authority"
            );
        }
    }
    let manifest = root.join("praxec.repo.yaml");
    let has_manifest = match fs::symlink_metadata(&manifest) {
        Ok(_) => true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => return Err(err).context("REPOS_MANIFEST: inspecting manifest"),
    };
    if stub && has_manifest {
        bail!("REPOS_MANIFEST_EXISTS: refusing to overwrite praxec.repo.yaml");
    }
    if has_manifest {
        praxec_core::repo::load_repo(&root).context("REPOS_MANIFEST: invalid repo")?;
    }
    if !has_manifest && !writable {
        bail!("REPOS_MANIFEST_REQUIRED: a manifest-free root requires --writable");
    }
    let relative = relative_path(&root, doc.path.parent().unwrap());
    let persisted_path = relative
        .to_str()
        .context("REPOS_PATH: local root must be representable as a UTF-8 YAML path")?;
    let mut entry = Mapping::new();
    entry.insert(Value::from("path"), Value::from(persisted_path));
    if let Some(name) = &name {
        entry.insert(Value::from("name"), Value::from(name.clone()));
    }
    if writable {
        entry.insert(Value::from("writable"), Value::Bool(true));
    }
    if push {
        entry.insert(Value::from("push"), Value::Bool(true));
    }
    if !has_manifest && !stub {
        entry.insert(Value::from("definitions"), Value::Bool(false));
    }
    entries.push(Value::Mapping(entry));
    let mut created_stub = if stub {
        Some(Stub::create(&root, name.as_deref())?)
    } else {
        None
    };
    let backup = doc.write(entries)?;
    if let Some(stub) = &mut created_stub {
        stub.committed = true;
    }
    println!(
        "added {} to {}; backup: {}",
        root.display(),
        doc.path.display(),
        backup.display()
    );
    Ok(())
}

fn remove(config: &Path, selector: &str) -> anyhow::Result<()> {
    let mut doc = Document::read(config)?;
    let _lock = EditLock::acquire(&doc.path)?;
    let mut entries = doc.entries()?;
    let selected_path = normalized(Path::new(selector))?;
    let relative_selector = normalized(&doc.path.parent().unwrap().join(selector))?;
    let matches = entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            let by_name = entry.get("name").and_then(Value::as_str) == Some(selector);
            match declared_path(entry, doc.path.parent().unwrap()) {
                Ok(Some(path)) if by_name || path == selected_path || path == relative_selector => {
                    Some(Ok(index))
                }
                Err(err) => Some(Err(err)),
                _ => None,
            }
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    match matches.as_slice() {
        [] => bail!("REPO_NOT_FOUND: no directly declared local path/name matches {selector:?}"),
        [index] => {
            entries.remove(*index);
        }
        _ => bail!(
            "REPO_SELECTOR_AMBIGUOUS: multiple entries match {selector:?}; use an unambiguous path/name"
        ),
    }
    let backup = doc.write(entries)?;
    println!(
        "removed declaration {selector:?}; repository files retained; backup: {}",
        backup.display()
    );
    Ok(())
}

/// Optional compatibility stub, with rollback if the gateway edit fails. Git's
/// own path resolver handles linked worktrees and avoids inventing a .git dir.
struct Stub {
    manifest: PathBuf,
    manifest_bytes: Vec<u8>,
    exclude: PathBuf,
    old_exclude: Option<Vec<u8>>,
    new_exclude: Vec<u8>,
    committed: bool,
}
impl Stub {
    fn create(root: &Path, name: Option<&str>) -> anyhow::Result<Self> {
        git_root(root)
            .context("REPOS_STUB_REQUIRES_GIT: --stub-manifest requires a Git worktree root")?;
        let tracked = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["ls-files", "--error-unmatch", "--", "praxec.repo.yaml"])
            .output()?;
        if tracked.status.success() {
            bail!(
                "REPOS_MANIFEST_TRACKED: restore the tracked manifest; a local exclude cannot hide a tracked file"
            );
        }
        if tracked.status.code() != Some(1) {
            bail!("REPOS_STUB: cannot verify whether the manifest is tracked");
        }
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args([
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "info/exclude",
            ])
            .output()?;
        if !output.status.success() {
            bail!(
                "REPOS_STUB_REQUIRES_GIT: --stub-manifest needs a Git checkout to exclude the generated file"
            );
        }
        let exclude = PathBuf::from(std::str::from_utf8(&output.stdout)?.trim());
        let old_exclude = match fs::read(&exclude) {
            Ok(bytes) => Some(bytes),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(err).context("REPOS_STUB: reading Git exclude"),
        };
        let mut new_exclude = old_exclude.clone().unwrap_or_default();
        if !String::from_utf8_lossy(&new_exclude)
            .lines()
            .any(|line| line == "/praxec.repo.yaml")
        {
            if !new_exclude.is_empty() && !new_exclude.ends_with(b"\n") {
                new_exclude.push(b'\n');
            }
            new_exclude.extend_from_slice(b"/praxec.repo.yaml\n");
        }
        let generated_name = format!("local-root-{}", uuid::Uuid::new_v4().simple());
        let name = name.unwrap_or(&generated_name);
        if !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            bail!(
                "INVALID_REPO_NAME: a stub identity must use lowercase letters, digits or hyphens"
            );
        }
        let empty_layout = format!("._none-{}", uuid::Uuid::new_v4().simple());
        let stub = serde_json::json!({
            "schema":"praxec.repo/v1", "name":name, "namespace":name, "version":"0.0.0",
            "layout": {"capabilities":empty_layout, "flows":empty_layout, "skills":empty_layout, "scripts":empty_layout, "connections":empty_layout},
            "reference_only": ["capabilities", "flows", "skills", "scripts", "connections"]
        });
        let manifest = root.join("praxec.repo.yaml");
        let bytes = serde_yaml::to_string(&stub)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&manifest)?;
        let result = (|| {
            file.write_all(bytes.as_bytes())?;
            file.sync_all()?;
            praxec_core::repo::load_manifest(root)?;
            if let Some(parent) = exclude.parent() {
                fs::create_dir_all(parent)?;
            }
            match &old_exclude {
                Some(old) => replace_file(&exclude, old, &new_exclude)?,
                None => create_file(&exclude, &new_exclude, &manifest)?,
            }
            Ok(())
        })();
        if let Err(err) = result {
            let _ = fs::remove_file(&manifest);
            return Err(err);
        }
        Ok(Self {
            manifest,
            manifest_bytes: bytes.into_bytes(),
            exclude,
            old_exclude,
            new_exclude,
            committed: false,
        })
    }
}

fn git_root(root: &Path) -> anyhow::Result<()> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--show-toplevel"])
        .output()?;
    if !output.status.success() {
        bail!("not a Git checkout");
    }
    let top = PathBuf::from(std::str::from_utf8(&output.stdout)?.trim()).canonicalize()?;
    if top != root {
        bail!(
            "path is inside {}, but is not its worktree root",
            top.display()
        );
    }
    Ok(())
}

impl Drop for Stub {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if fs::read(&self.manifest).ok().as_deref() == Some(self.manifest_bytes.as_slice()) {
            let _ = fs::remove_file(&self.manifest);
        }
        if fs::read(&self.exclude).ok().as_deref() == Some(self.new_exclude.as_slice()) {
            match &self.old_exclude {
                Some(old) => {
                    let _ = replace_file(&self.exclude, &self.new_exclude, old);
                }
                None => {
                    let _ = fs::remove_file(&self.exclude);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_external_edit_is_preserved() {
        let td = tempfile::tempdir().unwrap();
        let config = td.path().join("gateway.yaml");
        fs::write(&config, "version: '1.0.0'\n").unwrap();
        let mut doc = Document::read(&config).unwrap();
        fs::write(&config, "version: 'edited'\n").unwrap();
        assert!(
            doc.write(Vec::new())
                .unwrap_err()
                .to_string()
                .contains("REPOS_CONFIG_CHANGED")
        );
        assert_eq!(fs::read_to_string(config).unwrap(), "version: 'edited'\n");
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_root_is_rejected_before_config_edit() {
        use std::os::unix::ffi::OsStringExt;
        let td = tempfile::tempdir().unwrap();
        let config = td.path().join("gateway.yaml");
        fs::write(&config, "version: '1.0.0'\n").unwrap();
        let root = td
            .path()
            .join(std::ffi::OsString::from_vec(vec![b'r', 0xff]));
        fs::create_dir(&root).unwrap();
        let before = fs::read(&config).unwrap();
        let err = add(&config, &root, true, false, false, None).unwrap_err();
        assert!(err.to_string().contains("REPOS_PATH"));
        assert_eq!(fs::read(&config).unwrap(), before);
    }

    #[test]
    fn optional_stub_rolls_back_when_config_commit_fails() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("repo");
        fs::create_dir(&root).unwrap();
        assert!(
            std::process::Command::new("git")
                .arg("init")
                .arg(&root)
                .output()
                .unwrap()
                .status
                .success()
        );
        let exclude = root.join(".git/info/exclude");
        let old = fs::read(&exclude).unwrap();
        let config = td.path().join("gateway.yaml");
        fs::write(&config, "version: '1.0.0'\n").unwrap();
        let mut doc = Document::read(&config).unwrap();
        let stub = Stub::create(&root, Some("test-app")).unwrap();
        fs::write(&config, "version: 'changed'\n").unwrap();
        assert!(doc.write(Vec::new()).is_err());
        drop(stub);
        assert!(!root.join("praxec.repo.yaml").exists());
        assert_eq!(fs::read(&exclude).unwrap(), old);
    }

    #[cfg(unix)]
    #[test]
    fn config_edit_and_backup_retain_private_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let td = tempfile::tempdir().unwrap();
        let config = td.path().join("gateway.yaml");
        fs::write(&config, "version: '1.0.0'\n").unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
        let mut doc = Document::read(&config).unwrap();
        let backup = doc.write(Vec::new()).unwrap();
        for path in [&config, &backup] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
