//! Live proof — cpm-planner is obtained as a **prebuilt binary**, zero compilation.
//!
//! This is the end-to-end proof that the Windows dev's exact dead-end (the
//! companion MCP tools would not *compile*) is closed: a fresh gateway resolves
//! cpm-planner against a `praxec/packs`-shaped registry entry, "downloads" its
//! release asset, checksum-verifies it, and places the binary — and the
//! cargo/source path is **never** taken.
//!
//! Design: `docs/design/2026-08-01-onboarding-tool-provisioning.md` §6 (proof
//! tool: cpm-planner) + §10 (integration). cpm-planner itself needs no change —
//! it already publishes all-OS binaries + a GHCR image + a full `providers`
//! registry entry; this test is the consuming proof.
//!
//! The real resolve→verify→unpack→place chain runs against the host's **own**
//! target triple (`std::env::consts::{OS, ARCH}`), via a fake [`InstallerIo`]
//! that serves an in-memory asset + `checksums.sha256`. No network is touched.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use praxec_core::provision_install::{
    Consent, Host, InstallError, InstallOutcome, InstallerIo, Provider, asset_name, install,
    resolve_provider, resolve_target,
};
use praxec_core::registry_v3::Registry;

use sha2::{Digest, Sha256};

/// A `praxec/packs`-shaped registry entry for cpm-planner: the **release**
/// provider is a GitHub releases *page* (`{command}-{triple}.{ext}` asset shape
/// resolved by the installer), and docker + cargo are also declared — exactly
/// the real entry. If the chain ever preferred cargo/docker over the release
/// binary, this fixture would catch it.
const CPM_PLANNER_REGISTRY: &str = r#"schema: praxec.packs/v3
packs:
  - id: cpm-planner-pack
    name: CPM Planner pack
    namespace: cpm
    requires: [cpm-planner]
tools:
  - id: cpm-planner
    name: cpm-planner
    description: Critical-path planning MCP tool.
    command: cpm-planner
    version: 0.0.2
    mcp_registry_id: io.github.praxec/cpm-planner
    providers:
      release: https://github.com/praxec/cpm-planner/releases
      docker: ghcr.io/praxec/cpm-planner
      cargo: cpm-planner
"#;

const VERSION: &str = "0.0.2";

// ── the fake InstallerIo: in-memory, records every host-touching call ────────

/// Records every URL fetched, every executable placement, and every docker
/// pull. There is deliberately **no cargo seam** — the installer's cargo arm is
/// emit-only by construction (it never shells out), so "the cargo path was not
/// taken" is proven by the *absence* of any release/docker mutation plus a
/// `Release` resolution + an `Installed` (not `Offered`) outcome.
#[derive(Default)]
struct FakeIo {
    responses: HashMap<String, Vec<u8>>,
    bin_dir: PathBuf,
    gets: RefCell<Vec<String>>,
    writes: RefCell<Vec<(PathBuf, String)>>,
    pulls: RefCell<Vec<String>>,
}

impl InstallerIo for FakeIo {
    fn http_get(&self, url: &str) -> Result<Vec<u8>, InstallError> {
        self.gets.borrow_mut().push(url.to_string());
        self.responses
            .get(url)
            .cloned()
            .ok_or_else(|| InstallError::Io(format!("404 {url}")))
    }
    fn place_executable(
        &self,
        dir: &Path,
        name: &str,
        _bytes: &[u8],
    ) -> Result<PathBuf, InstallError> {
        self.writes
            .borrow_mut()
            .push((dir.to_path_buf(), name.to_string()));
        Ok(dir.join(name))
    }
    fn installed_version(&self, _dir: &Path, _name: &str) -> Option<String> {
        None // nothing placed yet → always proceed to install
    }
    fn bin_dir(&self) -> Result<PathBuf, InstallError> {
        Ok(self.bin_dir.clone())
    }
    fn which(&self, _cmd: &str) -> bool {
        // Both docker AND cargo report present, so the proof is strict: Release
        // must still win even when every lower-preference provider is available.
        true
    }
    fn docker_pull(&self, image_ref: &str) -> Result<(), InstallError> {
        self.pulls.borrow_mut().push(image_ref.to_string());
        Ok(())
    }
}

// ── archive builders (a genuine release asset the installer really unpacks) ──

fn make_targz(entry: &str, contents: &[u8]) -> Vec<u8> {
    let enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut builder = tar::Builder::new(enc);
    let mut header = tar::Header::new_gnu();
    header.set_size(contents.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    builder.append_data(&mut header, entry, contents).unwrap();
    builder.into_inner().unwrap().finish().unwrap()
}

fn make_zip(entry: &str, contents: &[u8]) -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut w = zip::ZipWriter::new(&mut buf);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        w.start_file(entry, opts).unwrap();
        w.write_all(contents).unwrap();
        w.finish().unwrap();
    }
    buf.into_inner()
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The release asset the host's own triple resolves to (`.tar.gz` on unix,
/// `.zip` on windows) carrying a `cpm-planner` executable entry — exactly what
/// the tool's `release.yml` publishes.
fn host_asset(triple: &str, ext: &str) -> (String, Vec<u8>) {
    let asset = asset_name("cpm-planner", triple, ext);
    let bytes = match ext {
        "zip" => make_zip("cpm-planner.exe", b"MZ fake windows binary"),
        _ => make_targz("cpm-planner", b"#!/bin/sh\necho cpm-planner\n"),
    };
    (asset, bytes)
}

/// The host this test runs on (its real triple must be one the tools publish).
fn this_host() -> Host {
    Host {
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
    }
}

// ── the proof ────────────────────────────────────────────────────────────────

/// The full happy path: a `praxec/packs`-shaped cpm-planner entry resolves to
/// the **Release** provider on this host and installs as a verified prebuilt
/// binary — with the cargo/source path provably untouched.
#[test]
fn cpm_planner_installs_as_a_prebuilt_binary_not_from_source() {
    let host = this_host();
    let (triple, ext) = resolve_target(&host.os, &host.arch)
        .expect("this test host must map to a published release triple");

    // Load the registry entry exactly as the runtime does (real v3 loader).
    let registry = Registry::load_str(CPM_PLANNER_REGISTRY).expect("fixture registry loads");
    let tool = registry
        .tool("cpm-planner")
        .expect("cpm-planner is in the registry");

    // Serve the good asset + a matching checksums.sha256 through the fake IO.
    let page = "https://github.com/praxec/cpm-planner/releases";
    let (asset, asset_bytes) = host_asset(triple, ext);
    let mut responses = HashMap::new();
    responses.insert(
        format!("{page}/download/v{VERSION}/{asset}"),
        asset_bytes.clone(),
    );
    responses.insert(
        format!("{page}/download/v{VERSION}/checksums.sha256"),
        format!("{}  {asset}\n", sha256_hex(&asset_bytes)).into_bytes(),
    );
    let io = FakeIo {
        responses,
        bin_dir: PathBuf::from("/fake/praxec/bin"),
        ..Default::default()
    };

    // (a) resolves to Release — NOT docker, NOT cargo — even though the fake IO
    //     reports both docker and cargo present.
    let plan = resolve_provider(tool, &host, &io).expect("a provider resolves");
    assert_eq!(
        plan.provider,
        Provider::Release,
        "release binary must be the chosen provider (never source/cargo)"
    );

    // (b) the asset is checksum-verified and the binary is placed.
    let outcome = install(tool, &host, Consent::Granted, &io).expect("install succeeds");
    assert_eq!(
        outcome,
        InstallOutcome::Installed {
            path: PathBuf::from("/fake/praxec/bin").join("cpm-planner"),
            version: VERSION.to_string(),
        },
        "cpm-planner is placed as a prebuilt binary"
    );
    assert_eq!(io.writes.borrow().len(), 1, "exactly one binary placed");
    assert_eq!(io.writes.borrow()[0].1, "cpm-planner");

    // (c) the cargo/source path is NEVER taken — the Windows compile dead-end is
    //     closed. The installer has no cargo execution seam at all (cargo is
    //     emit-only by construction), so the proof is: Release resolved, the
    //     outcome is a placed binary (not `Offered{Cargo}`), zero docker pulls,
    //     and every URL fetched was a release-asset URL — no crates.io / cargo
    //     fetch, no source build.
    assert!(
        !matches!(outcome, InstallOutcome::Offered { .. }),
        "an Offered outcome would mean the cargo emit-only arm was taken"
    );
    assert_eq!(
        io.pulls.borrow().len(),
        0,
        "docker must not be pulled (release wins)"
    );
    let gets = io.gets.borrow();
    assert_eq!(
        gets.len(),
        2,
        "only the asset + its checksums were fetched: {gets:?}"
    );
    for url in gets.iter() {
        assert!(
            url.contains("/releases/download/")
                && !url.contains("crates.io")
                && !url.contains("cargo"),
            "every fetch is a release-asset URL, never a cargo/source fetch: {url}"
        );
    }
}

/// A tampered/corrupt asset (its bytes do not match `checksums.sha256`) is
/// **refused** and no binary is placed — integrity is enforced at install
/// regardless of how the registry was sourced (design §5 / FMECA §9).
#[test]
fn cpm_planner_corrupt_asset_is_refused_and_not_placed() {
    let host = this_host();
    let (triple, ext) = resolve_target(&host.os, &host.arch)
        .expect("this test host must map to a published release triple");

    let registry = Registry::load_str(CPM_PLANNER_REGISTRY).expect("fixture registry loads");
    let tool = registry
        .tool("cpm-planner")
        .expect("cpm-planner is in the registry");

    let page = "https://github.com/praxec/cpm-planner/releases";
    let (asset, asset_bytes) = host_asset(triple, ext);
    let mut responses = HashMap::new();
    // The asset is served, but the checksum line is for DIFFERENT bytes → the
    // downloaded asset fails verification.
    responses.insert(format!("{page}/download/v{VERSION}/{asset}"), asset_bytes);
    responses.insert(
        format!("{page}/download/v{VERSION}/checksums.sha256"),
        format!(
            "{}  {asset}\n",
            sha256_hex(b"a different, tampered artifact")
        )
        .into_bytes(),
    );
    let io = FakeIo {
        responses,
        bin_dir: PathBuf::from("/fake/praxec/bin"),
        ..Default::default()
    };

    let outcome = install(tool, &host, Consent::Granted, &io).expect("install returns a decision");
    assert!(
        matches!(outcome, InstallOutcome::Refused { .. }),
        "a checksum mismatch must be Refused, got {outcome:?}"
    );
    assert_eq!(
        io.writes.borrow().len(),
        0,
        "no binary is placed when integrity verification fails"
    );
}
