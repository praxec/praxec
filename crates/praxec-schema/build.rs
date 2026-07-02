use std::{fs, path::PathBuf};

use anyhow::Context;

fn main() -> anyhow::Result<()> {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let schemas_dir = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("schemas"))
        .context("locating schemas directory")?;

    let inputs = [
        "gateway-config.schema.json",
        "transition-record.schema.json",
        "workflow-response.schema.json",
    ];

    let settings = typify::TypeSpaceSettings::default();
    let mut type_space = typify::TypeSpace::new(&settings);

    for name in inputs {
        let path = schemas_dir.join(name);
        println!("cargo:rerun-if-changed={}", path.display());

        let text = fs::read_to_string(&path)
            .with_context(|| format!("reading schema {}", path.display()))?;
        let schema: schemars::schema::RootSchema = serde_json::from_str(&text)
            .with_context(|| format!("parsing schema {}", path.display()))?;
        type_space
            .add_root_schema(schema)
            .with_context(|| format!("adding schema {}", path.display()))?;
    }

    let stream = type_space.to_stream();
    let file = syn::parse2::<syn::File>(stream).context("parsing generated tokens")?;
    let pretty = prettyplease::unparse(&file);

    let out_dir = PathBuf::from(std::env::var("OUT_DIR")?);
    let out_path = out_dir.join("types.rs");
    fs::write(&out_path, pretty).with_context(|| format!("writing {}", out_path.display()))?;

    Ok(())
}
