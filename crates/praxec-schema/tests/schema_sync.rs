//! The packaged schema copies MUST be byte-identical to the repo-root
//! authored sources — the crate ships the copies, the workspace edits the
//! originals, and this test is the only thing keeping them the same file.
use std::{fs, path::PathBuf};

#[test]
fn packaged_schemas_match_the_repo_root_sources() {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let Some(root) = crate_dir.parent().and_then(|p| p.parent()) else {
        return; // packaged build: no repo root — the copies ARE the source
    };
    let root_schemas = root.join("schemas");
    if !root_schemas.exists() {
        return; // packaged build layout
    }
    for name in [
        "gateway-config.schema.json",
        "transition-record.schema.json",
        "workflow-response.schema.json",
    ] {
        let ours = fs::read(crate_dir.join("schemas").join(name)).expect(name);
        let theirs = fs::read(root_schemas.join(name)).expect(name);
        assert_eq!(ours, theirs, "{name} drifted — re-copy it into crates/praxec-schema/schemas/");
    }
}
