//! `static`/`direct` registry adapter — the config's own inline
//! `candidates:` list, passed through verbatim except for a stamped
//! `provenance` (the registry's own `name:`, so dedup/tie-break logic in
//! [`crate::tool_catalog::catalog`] always has a source to attribute to,
//! regardless of what the config author put in each candidate).

use crate::tool_catalog::candidate::ToolCandidate;
use crate::tool_catalog::registry::{CatalogIo, RegistryAdapter};

/// Serves the inline candidates of one `kind: static` registry entry.
pub struct StaticAdapter {
    name: String,
    candidates: Vec<ToolCandidate>,
}

impl StaticAdapter {
    pub fn new(name: impl Into<String>, candidates: Vec<ToolCandidate>) -> Self {
        Self {
            name: name.into(),
            candidates,
        }
    }
}

impl RegistryAdapter for StaticAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    /// No IO needed — the candidates are already inline in config. Clones
    /// them, stamping `provenance` with this registry's name.
    fn candidates(&self, _io: &dyn CatalogIo) -> Result<Vec<ToolCandidate>, String> {
        Ok(self
            .candidates
            .iter()
            .cloned()
            .map(|mut c| {
                c.provenance = self.name.clone();
                c
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_catalog::candidate::{Requires, ToolSource, Transport, TrustTier};
    use serde_json::Value;

    /// A `CatalogIo` that never gets called — the static adapter needs none.
    struct NoopIo;
    impl CatalogIo for NoopIo {
        fn github_org_repos(
            &self,
            _org: &str,
        ) -> Result<Vec<crate::tool_catalog::registry::GhRepo>, String> {
            Err("unused".into())
        }
        fn fetch_json(&self, _url: &str) -> Result<Value, String> {
            Err("unused".into())
        }
    }

    fn a_candidate() -> ToolCandidate {
        ToolCandidate {
            name: "x".into(),
            description: "desc".into(),
            transport: Transport::Stdio,
            source: ToolSource::Crate { name: "x".into() },
            verbs: vec![],
            tags: vec![],
            trust_tier: TrustTier::Community,
            requires: Requires::default(),
            provenance: "".into(),
        }
    }

    #[test]
    fn static_adapter_returns_its_candidates_with_provenance() {
        let a = StaticAdapter::new("local", vec![a_candidate()]);
        let out = a.candidates(&NoopIo).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].provenance, "local"); // stamped by the adapter
    }
}
