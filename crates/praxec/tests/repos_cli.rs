//! #272: actual on-disk operator workflow, including removed worktrees.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn run(config: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_praxec"))
        .args(args)
        .arg("--config")
        .arg(config)
        .current_dir(config.parent().unwrap())
        .output()
        .unwrap()
}
fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}
fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("gateway.yaml");
    let root = dir.path().join("code");
    fs::create_dir(&root).unwrap();
    fs::write(&config, "# operator comment\nversion: '1.0.0'\ngateway:\n  allow_ephemeral: true\ncustom:\n  keep: exact-value\n").unwrap();
    (dir, config, root)
}
fn value(config: &Path) -> serde_json::Value {
    serde_yaml::from_str(&fs::read_to_string(config).unwrap()).unwrap()
}
fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

#[test]
fn add_list_remove_plain_root_preserves_config_and_repository() {
    let (_dir, config, root) = setup();
    let original = fs::read(&config).unwrap();
    let out = run(
        &config,
        &["repos", "add", "code", "--writable", "--name", "app"],
    );
    assert!(out.status.success(), "{}", text(&out));
    let doc = value(&config);
    assert_eq!(doc["repos"][0]["path"], "code");
    assert_eq!(doc["repos"][0]["definitions"], false);
    assert_eq!(doc["repos"][0]["writable"], true);
    assert!(doc["repos"][0].get("push").is_none());
    assert_eq!(doc["custom"]["keep"], "exact-value");
    assert!(!root.join("praxec.repo.yaml").exists());
    let backups: Vec<_> = fs::read_dir(config.parent().unwrap())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".bak"))
        .collect();
    assert_eq!(backups.len(), 1);
    assert_eq!(fs::read(backups[0].path()).unwrap(), original);
    let out = run(&config, &["repos", "list"]);
    assert!(out.status.success(), "{}", text(&out));
    let listed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(listed, doc["repos"]);
    assert!(run(&config, &["check"]).status.success());
    assert!(run(&config, &["repos", "remove", "app"]).status.success());
    assert_eq!(value(&config)["repos"], serde_json::json!([]));
    assert!(root.is_dir());
}

#[test]
fn duplicate_path_or_name_is_refused_without_config_edit() {
    let (dir, config, root) = setup();
    assert!(
        run(
            &config,
            &["repos", "add", "code", "--writable", "--name", "app"]
        )
        .status
        .success()
    );
    let before = fs::read(&config).unwrap();
    let out = run(
        &config,
        &["repos", "add", root.to_str().unwrap(), "--writable"],
    );
    assert!(!out.status.success() && text(&out).contains("DUPLICATE_REPO"));
    fs::create_dir(dir.path().join("other")).unwrap();
    assert!(
        !run(
            &config,
            &["repos", "add", "other", "--writable", "--name", "app"]
        )
        .status
        .success()
    );
    assert_eq!(fs::read(&config).unwrap(), before);
}

#[test]
fn missing_and_readonly_manifest_free_additions_fail_before_writing() {
    let (_dir, config, _root) = setup();
    let original = fs::read(&config).unwrap();
    for args in [
        vec!["repos", "add", "gone", "--writable"],
        vec!["repos", "add", "code"],
        vec!["repos", "add", "code", "--push"],
    ] {
        let out = run(&config, &args);
        assert!(!out.status.success(), "{}", text(&out));
        assert_eq!(fs::read(&config).unwrap(), original);
    }
}

#[test]
fn optional_stub_is_zero_definition_git_excluded_and_never_overwrites() {
    let (_dir, config, root) = setup();
    git(&root, &["init"]);
    let out = run(
        &config,
        &[
            "repos",
            "add",
            "code",
            "--writable",
            "--stub-manifest",
            "--name",
            "local-app",
        ],
    );
    assert!(out.status.success(), "{}", text(&out));
    let (manifest, defs) = praxec_core::repo::load_repo(&root).unwrap();
    assert_eq!(manifest.name, "local-app");
    assert!(
        defs.get("workflows")
            .is_none_or(|w| w.as_object().unwrap().is_empty())
    );
    assert_eq!(
        git(&root, &["check-ignore", "praxec.repo.yaml"]),
        "praxec.repo.yaml"
    );
    let before = fs::read(root.join("praxec.repo.yaml")).unwrap();
    assert!(run(&config, &["repos", "remove", "code"]).status.success());
    let out = run(
        &config,
        &["repos", "add", "code", "--writable", "--stub-manifest"],
    );
    assert!(!out.status.success() && text(&out).contains("REPOS_MANIFEST_EXISTS"));
    assert_eq!(fs::read(root.join("praxec.repo.yaml")).unwrap(), before);
}

#[test]
fn stub_requires_git_and_leaves_config_and_directory_untouched() {
    let (_dir, config, root) = setup();
    let before = fs::read(&config).unwrap();
    let out = run(
        &config,
        &["repos", "add", "code", "--writable", "--stub-manifest"],
    );
    assert!(!out.status.success() && text(&out).contains("REPOS_STUB_REQUIRES_GIT"));
    assert_eq!(fs::read(&config).unwrap(), before);
    assert!(!root.join("praxec.repo.yaml").exists());
}

#[test]
fn stub_does_not_replace_a_deleted_tracked_manifest() {
    let (_dir, config, root) = setup();
    git(&root, &["init"]);
    let manifest = root.join("praxec.repo.yaml");
    fs::write(&manifest, "tracked original\n").unwrap();
    git(&root, &["add", "praxec.repo.yaml"]);
    fs::remove_file(&manifest).unwrap();
    let before = fs::read(&config).unwrap();
    let out = run(
        &config,
        &["repos", "add", "code", "--writable", "--stub-manifest"],
    );
    assert!(
        !out.status.success() && text(&out).contains("REPOS_MANIFEST_TRACKED"),
        "{}",
        text(&out)
    );
    assert_eq!(fs::read(&config).unwrap(), before);
    assert!(!manifest.exists());
}

#[test]
fn corrupt_manifest_is_rejected_instead_of_becoming_root_only() {
    let (_dir, config, root) = setup();
    fs::write(root.join("praxec.repo.yaml"), "schema: broken\n").unwrap();
    let before = fs::read(&config).unwrap();
    let out = run(&config, &["repos", "add", "code", "--writable"]);
    assert!(!out.status.success() && text(&out).contains("REPOS_MANIFEST"));
    assert_eq!(fs::read(&config).unwrap(), before);
}

#[test]
fn removed_worktree_can_be_removed_by_its_missing_path() {
    let (_dir, config, root) = setup();
    git(&root, &["init"]);
    assert!(
        run(&config, &["repos", "add", "code", "--writable", "--push"])
            .status
            .success()
    );
    assert_eq!(value(&config)["repos"][0]["push"], true);
    fs::remove_dir_all(&root).unwrap();
    assert!(run(&config, &["repos", "remove", "code"]).status.success());
}

#[test]
fn include_config_is_not_silently_masked() {
    let (dir, config, _root) = setup();
    fs::write(&config, "include: [base.yaml]\nversion: '1.0.0'\n").unwrap();
    fs::write(dir.path().join("base.yaml"), "repos: []\n").unwrap();
    let before = fs::read(&config).unwrap();
    let out = run(&config, &["repos", "add", "code", "--writable"]);
    assert!(!out.status.success() && text(&out).contains("REPOS_INCLUDED_CONFIG"));
    assert_eq!(fs::read(&config).unwrap(), before);
}

#[test]
fn locked_edits_are_refused_and_unknown_remove_does_not_write() {
    let (_dir, config, _root) = setup();
    let before = fs::read(&config).unwrap();
    let lock = config.parent().unwrap().join(".gateway.yaml.repos.lock");
    fs::write(&lock, "pid=active\n").unwrap();
    let out = run(&config, &["repos", "add", "code", "--writable"]);
    assert!(!out.status.success() && text(&out).contains("REPOS_EDIT_LOCKED"));
    assert_eq!(fs::read(&config).unwrap(), before);
    fs::remove_file(lock).unwrap();
    let out = run(&config, &["repos", "remove", "unknown"]);
    assert!(!out.status.success() && text(&out).contains("REPO_NOT_FOUND"));
    assert_eq!(fs::read(&config).unwrap(), before);
}

#[test]
fn stub_works_in_linked_worktree_and_rejects_subdirectories() {
    let (dir, config, root) = setup();
    git(&root, &["init"]);
    git(
        &root,
        &[
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--allow-empty",
            "-m",
            "seed",
        ],
    );
    let linked = dir.path().join("linked");
    git(
        &root,
        &[
            "worktree",
            "add",
            "-b",
            "linked-test",
            linked.to_str().unwrap(),
        ],
    );
    let out = run(
        &config,
        &["repos", "add", "linked", "--writable", "--stub-manifest"],
    );
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(
        git(&linked, &["check-ignore", "praxec.repo.yaml"]),
        "praxec.repo.yaml"
    );
    fs::create_dir(root.join("nested")).unwrap();
    let out = run(
        &config,
        &[
            "repos",
            "add",
            "code/nested",
            "--writable",
            "--stub-manifest",
        ],
    );
    assert!(!out.status.success() && text(&out).contains("REPOS_STUB_REQUIRES_GIT"));
    assert!(!root.join("nested/praxec.repo.yaml").exists());
}

#[test]
fn list_and_local_edit_preserve_remote_declarations_without_fetching() {
    let (_dir, config, _root) = setup();
    fs::write(&config, "version: '1.0.0'\nrepos:\n  - uri: git+https://example.invalid/offline.git\n    ref: dev\n    grant_connections: [safe]\n").unwrap();
    let out = run(&config, &["repos", "list"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        run(&config, &["repos", "add", "code", "--writable"])
            .status
            .success()
    );
    let doc = value(&config);
    assert_eq!(
        doc["repos"][0]["uri"],
        "git+https://example.invalid/offline.git"
    );
    assert_eq!(doc["repos"][0]["grant_connections"][0], "safe");
    assert!(run(&config, &["repos", "remove", "code"]).status.success());
    assert_eq!(value(&config)["repos"].as_array().unwrap().len(), 1);
}

#[cfg(unix)]
#[test]
fn adding_through_config_symlink_loads_the_same_canonical_root() {
    let (dir, config, root) = setup();
    let alias_dir = dir.path().join("alias/deep");
    fs::create_dir_all(&alias_dir).unwrap();
    let alias = alias_dir.join("gateway.yaml");
    std::os::unix::fs::symlink(&config, &alias).unwrap();
    let out = run(
        &alias,
        &[
            "repos",
            "add",
            root.to_str().unwrap(),
            "--writable",
            "--name",
            "app",
        ],
    );
    assert!(out.status.success(), "{}", text(&out));
    assert!(fs::symlink_metadata(&alias).unwrap().is_symlink());
    assert_eq!(value(&config)["repos"][0]["path"], "code");
    let (resolved, diagnostics) = praxec_core::config::load_resolved_with_repos(&alias).unwrap();
    assert!(!diagnostics.iter().any(|d| d.code == "REPO_ROOT_MISSING"));
    assert_eq!(
        resolved["praxec"]["_writableRepos"][0]["root"],
        root.to_str().unwrap()
    );
    let out = run(&alias, &["health"]);
    assert!(
        out.status.success() && !text(&out).contains("REPO_ROOT_MISSING"),
        "{}",
        text(&out)
    );
    assert!(run(&alias, &["repos", "remove", "app"]).status.success());
}

#[test]
fn preflight_surfaces_each_dead_root_while_live_pack_remains_available() {
    let (dir, config, root) = setup();
    fs::write(
        root.join("praxec.repo.yaml"),
        "schema: praxec.repo/v1\nname: app\nnamespace: app\nversion: 0.0.0\n",
    )
    .unwrap();
    fs::write(&config, "version: '1.0.0'\ngateway: {allow_ephemeral: true}\nrepos:\n  - path: code\n  - path: gone-a\n  - path: gone-b\n    writable: true\n    definitions: false\n").unwrap();
    for command in ["check", "doctor", "health"] {
        let out = run(&config, &[command]);
        assert!(out.status.success(), "{command}: {}", text(&out));
        assert!(text(&out).contains("gone-a") && text(&out).contains("gone-b"));
        assert!(text(&out).contains("REPO_ROOT_MISSING"));
        if command == "health" {
            let health: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
            assert_eq!(
                health["diagnostics"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|d| d["code"] == "REPO_ROOT_MISSING")
                    .count(),
                2
            );
        }
    }
    assert!(!dir.path().join("gone-a").exists());
}

#[test]
fn all_definition_roots_missing_fails_with_aggregate_remedies() {
    let (_dir, config, _root) = setup();
    fs::write(
        &config,
        "version: '1.0.0'\nrepos:\n  - path: gone-a\n  - path: gone-b\n",
    )
    .unwrap();
    for command in ["check", "doctor", "health"] {
        let out = run(&config, &[command]);
        assert!(!out.status.success());
        let output = text(&out);
        assert!(
            output.contains("ALL_REPOS_FAILED")
                && output.contains("gone-a")
                && output.contains("gone-b")
                && output.contains("remove/comment"),
            "{output}"
        );
    }
}
