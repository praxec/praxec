//! Capability discovery and search.
//!
//! Discovery is a separate HATEOAS layer from the workflow runtime: a model
//! starts at `gateway.home`, calls `gateway.search` to find a relevant
//! workflow or proxy capability, follows the returned link to start it, and
//! from there is in workflow-HATEOAS land.
//!
//! The MVP uses an in-memory lexical scorer over a flat `Vec<DiscoveryItem>`.
//! The trait is async so backends like Tantivy or vector indexes can plug in
//! later without changing callers.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryKind {
    Workflow,
    Capability,
    Connection,
    /// A reusable guidance fragment ("skill"). The lookup id is the fragment's
    /// `subject`; `gateway.describe(subject)` returns its `verb` + `body`.
    Guidance,
    /// SPEC §22 — a curated, hash-pinned script body invokable by a workflow's
    /// `script` executor. The lookup id is the script's `subject`;
    /// `gateway.describe(subject)` returns its `verb` + `body` (the executable
    /// content). Distinct from `Guidance` because scripts have stricter
    /// hash normalization (whitespace matters in shell) and a separate verb
    /// vocabulary (build/test/deploy/... vs triage/diagnose/plan/...).
    Script,
    /// ADR-0007 — a first-class agent: the *engine* (model binding + harness
    /// config) that orchestrates a workflow. Discoverable + launchable like a
    /// skill or script; the lookup id is the agent's declared name.
    Agent,
}

impl DiscoveryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DiscoveryKind::Workflow => "workflow",
            DiscoveryKind::Capability => "capability",
            DiscoveryKind::Connection => "connection",
            DiscoveryKind::Guidance => "guidance",
            DiscoveryKind::Script => "script",
            DiscoveryKind::Agent => "agent",
        }
    }
}

/// SPEC §5.4.1 — the eight closed cognitive-operation verbs that may tag a
/// guidance fragment. This is a closed enum on purpose: no `Other(String)`
/// escape variant, no `#[serde(other)]`. Authoring a new verb requires a
/// deliberate spec amendment, not a config-time string. Unknown verbs fail
/// config-load with `INVALID_VERB` (see [`Verb::from_token`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verb {
    /// Classify, prioritize, route.
    Triage,
    /// Find root cause.
    Diagnose,
    /// Design approach before acting.
    Plan,
    /// Produce / generate the artifact.
    Implement,
    /// Evaluate against criteria.
    Review,
    /// Restructure preserving behavior.
    Refactor,
    /// Build understanding (self-explain or teach others).
    Explain,
    /// Assemble parts into a whole.
    Compose,
    /// SPEC §5.4.1 — Gather context from sources (web, local, docs).
    /// Distinct from `diagnose` (root-cause) — `research` is open-ended
    /// information-gathering; `diagnose` answers a specific "why" question.
    Research,
    /// SPEC §5.4.1 — Condense. Distinct from `explain` (which builds
    /// understanding via expansion) — `summarize` compresses what is
    /// already understood.
    Summarize,
}

impl Verb {
    /// The closed set of allowed verb tokens, in spec order. Returned as
    /// `&'static [&'static str]` so error messages can list them verbatim
    /// without per-call allocation.
    pub const ALL_TOKENS: &'static [&'static str] = &[
        "triage",
        "diagnose",
        "plan",
        "implement",
        "review",
        "refactor",
        "explain",
        "compose",
        "research",
        "summarize",
    ];

    pub fn as_token(self) -> &'static str {
        match self {
            Verb::Triage => "triage",
            Verb::Diagnose => "diagnose",
            Verb::Plan => "plan",
            Verb::Implement => "implement",
            Verb::Review => "review",
            Verb::Refactor => "refactor",
            Verb::Explain => "explain",
            Verb::Compose => "compose",
            Verb::Research => "research",
            Verb::Summarize => "summarize",
        }
    }

    /// Parse a verb token, case-sensitively. Returns `None` for any string not
    /// in the closed set. Whitespace, uppercase, hyphen, dot, or any other
    /// deviation rejects.
    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "triage" => Some(Verb::Triage),
            "diagnose" => Some(Verb::Diagnose),
            "plan" => Some(Verb::Plan),
            "implement" => Some(Verb::Implement),
            "review" => Some(Verb::Review),
            "refactor" => Some(Verb::Refactor),
            "explain" => Some(Verb::Explain),
            "compose" => Some(Verb::Compose),
            "research" => Some(Verb::Research),
            "summarize" => Some(Verb::Summarize),
            _ => None,
        }
    }
}

/// SPEC §5.3 — required lifecycle marker on every guidance fragment. Closed
/// enum; no silent default. A fragment without `lifecycle` fails config-load
/// with `MISSING_LIFECYCLE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lifecycle {
    Experimental,
    Stable,
    Deprecated,
}

impl Lifecycle {
    pub const ALL_TOKENS: &'static [&'static str] = &["experimental", "stable", "deprecated"];

    pub fn as_token(self) -> &'static str {
        match self {
            Lifecycle::Experimental => "experimental",
            Lifecycle::Stable => "stable",
            Lifecycle::Deprecated => "deprecated",
        }
    }

    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "experimental" => Some(Lifecycle::Experimental),
            "stable" => Some(Lifecycle::Stable),
            "deprecated" => Some(Lifecycle::Deprecated),
            _ => None,
        }
    }
}

/// SPEC §5.4.2 — the blessed top-level segments for guidance fragment
/// subjects. A subject's first dotted segment MUST be one of these, or the
/// config produces an `INVALID_SUBJECT_ROOT` diagnostic (error under
/// `strict_namespacing: true`, warning otherwise).
///
/// The list combines:
/// - Six domain-themed roots (`authoring`, `debug`, `deploy`, `import`,
///   `lifecycle`, `review`) that group guidance by topic regardless of
///   which verb is appropriate.
/// - Eight verb-mirror roots — one per closed verb in [`Verb::ALL_TOKENS`] —
///   so authors can group guidance by the cognitive operation it primes
///   (e.g. `implement.edit.constrained`, `diagnose.codebase.search`).
///
/// Two roots (`plan` and `review`) appear in BOTH categories; they are
/// listed once. Total: 12 blessed roots.
pub const BLESSED_SUBJECT_ROOTS: &[&str] = &[
    // Domain-themed (groups guidance by topic).
    "authoring",
    "debug",
    "deploy",
    "import",
    "lifecycle",
    // Verb-mirror (groups guidance by cognitive operation).
    "triage",
    "diagnose",
    "plan", // also a verb
    "implement",
    "review", // also a verb
    "refactor",
    "explain",
    "compose",
    // SPEC §5.4.1 expansion (v0.3) — verb-mirror roots for the
    // reconnaissance + condensation verbs.
    "research",
    "summarize",
];

/// SPEC §22.3 — the eight closed action verbs that may tag a curated script.
/// Distinct from [`Verb`] (cognitive verbs) because scripts perform actions,
/// not cognition: a Bash script doesn't `triage` or `explain`, it builds,
/// tests, deploys, etc. Closed enum on purpose; new verbs require a spec
/// amendment. Unknown verbs fail config-load with `INVALID_SCRIPT_VERB`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScriptVerb {
    /// Compile, package, generate artifacts.
    Build,
    /// Exercise the system against assertions (unit / integration / e2e).
    Test,
    /// Promote artifacts to an environment.
    Deploy,
    /// Apply style transformations (write changes; not check).
    Format,
    /// Inspect for static issues (read-only; report don't fix).
    Lint,
    /// Provision dependencies, toolchains, runtimes.
    Install,
    /// Confirm an externally-asserted property (hash match, signature, contract).
    Verify,
    /// Catch-all for runnable operations that don't fit the above (smoke
    /// runs, ad-hoc helpers). Use sparingly — prefer a more specific verb.
    Run,
    /// SPEC §22.3 expansion (v0.3) — read-only local introspection.
    /// System state, dep trees, symbol exports, env. Distinct from `lint`
    /// (binary pass/fail on issues) and `run` (loses semantic info).
    Inspect,
    /// SPEC §22.3 expansion (v0.3) — content discovery: codebase grep,
    /// web search, doc search. Distinct from `fetch` (known resource by
    /// id) and `inspect` (system state, not content).
    Search,
    /// SPEC §22.3 expansion (v0.3) — retrieve a specific known resource
    /// by URL or path. Distinct from `search` (discovery vs known
    /// retrieval).
    Fetch,
    /// SPEC §22.3 expansion (v0.3) — graded compliance / security /
    /// quality scan. Emits structured findings. Distinct from `lint`
    /// (binary pass/fail) — `audit` is a report.
    Audit,
}

impl ScriptVerb {
    pub const ALL_TOKENS: &'static [&'static str] = &[
        "build", "test", "deploy", "format", "lint", "install", "verify", "run", "inspect",
        "search", "fetch", "audit",
    ];

    pub fn as_token(self) -> &'static str {
        match self {
            ScriptVerb::Build => "build",
            ScriptVerb::Test => "test",
            ScriptVerb::Deploy => "deploy",
            ScriptVerb::Format => "format",
            ScriptVerb::Lint => "lint",
            ScriptVerb::Install => "install",
            ScriptVerb::Verify => "verify",
            ScriptVerb::Run => "run",
            ScriptVerb::Inspect => "inspect",
            ScriptVerb::Search => "search",
            ScriptVerb::Fetch => "fetch",
            ScriptVerb::Audit => "audit",
        }
    }

    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "build" => Some(ScriptVerb::Build),
            "test" => Some(ScriptVerb::Test),
            "deploy" => Some(ScriptVerb::Deploy),
            "format" => Some(ScriptVerb::Format),
            "lint" => Some(ScriptVerb::Lint),
            "install" => Some(ScriptVerb::Install),
            "verify" => Some(ScriptVerb::Verify),
            "run" => Some(ScriptVerb::Run),
            "inspect" => Some(ScriptVerb::Inspect),
            "search" => Some(ScriptVerb::Search),
            "fetch" => Some(ScriptVerb::Fetch),
            "audit" => Some(ScriptVerb::Audit),
            _ => None,
        }
    }
}

/// SPEC §22.4 — blessed top-level segments for script subjects. Mirrors
/// [`BLESSED_SUBJECT_ROOTS`] but the vocabulary is action-flavored. Combines
/// the eight [`ScriptVerb`] tokens as verb-mirror roots with three
/// domain-themed extensions (`release`, `migrate`, `ci`) for common
/// operational categories. `strict_namespacing: true` (default) rejects
/// unblessed roots with `INVALID_SCRIPT_SUBJECT_ROOT`; lenient mode warns
/// with the closest-blessed-root suggestion.
pub const BLESSED_SCRIPT_ROOTS: &[&str] = &[
    // Verb-mirror (action category).
    "build", "test", "deploy", "format", "lint", "install", "verify", "run",
    // SPEC §22.3 expansion (v0.3) — verb-mirror roots for the
    // reconnaissance + graded-findings verbs.
    "inspect", "search", "fetch", "audit", // Domain-themed (operational category).
    "release", "migrate", "ci",
];

/// A single thing that can be discovered: a workflow, a proxy capability, or
/// a configured connection. Everything carries enough metadata to score it
/// against a query and to render a HATEOAS link template that lets the caller
/// act on it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryItem {
    pub id: String,
    pub kind: DiscoveryKind,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub examples: Vec<String>,
    /// Author-provided synonyms. Indexed with the same weight as tags so a
    /// capability named `release.promote` can declare `aliases: ["deploy", "ship"]`
    /// and be found by those terms.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Free-form text that lexical scoring can search over. Index-builders
    /// fill this with concatenated state names, transition names, etc.
    #[serde(default)]
    pub text: String,
    /// HATEOAS templates for what to do with this item.
    #[serde(default)]
    pub links: Vec<DiscoveryLink>,
    /// Guidance fragments only: the fragment's space-free `verb` (`apply`,
    /// `check`, ...). `None` for non-guidance items.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verb: Option<String>,
    /// Guidance fragments only: the fragment's static markdown body returned
    /// by `gateway.describe`. `None` for non-guidance items.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// SPEC §5.3 — fragment provenance. Examples: `config` (declared inline
    /// in workflow YAML), `git+https://github.com/org/repo@sha`. Used by the
    /// `gateway.skills.search` `source` filter (§17.6). `None` for
    /// non-guidance items.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// The tag prefix that carries a flow's `process` / `taskClass` into the
/// (existing) searchable `tags`, so the catalog is filterable by task-class
/// without a new typed field on every `DiscoveryItem` constructor. Read it back
/// via [`DiscoveryItem::task_class`].
pub const PROCESS_TAG_PREFIX: &str = "process:";

impl DiscoveryItem {
    /// The flow's declared process / task-class, if tagged — the unit the intent
    /// index keys on and the Phase-3 selector filters by. `None` for untagged
    /// items (raw tools, skills, capabilities, unclassified flows).
    pub fn task_class(&self) -> Option<&str> {
        self.tags
            .iter()
            .find_map(|t| t.strip_prefix(PROCESS_TAG_PREFIX))
            .filter(|s| !s.is_empty())
    }
}

/// A pre-built HATEOAS link attached to a `DiscoveryItem`. These are
/// "next-step" pointers — typically `workflow.start` for a workflow or a
/// `workflow.start` against `proxy_default` for a capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryLink {
    pub rel: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// MCP tool name to call (`workflow.start`, `gateway.search`, ...).
    pub method: String,
    /// Pre-filled arguments for that tool call.
    pub args: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub score: f32,
    pub item: DiscoveryItem,
    /// Workflow hits only: the intent-index track record for this template
    /// within its declared task-class (`{runs, success_rate, mean_cost_usd}`),
    /// attached by [`crate::intent_index::annotate_hits_with_evidence`] so a
    /// caller picks by evidence, not blind. Omitted when the sample is thinner
    /// than the tuning `intent.min_runs` gate, when no audit history is
    /// readable, or for non-workflow / unclassified hits — missing evidence is
    /// the normal state of a fresh system, never an error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<crate::intent_index::IntentEvidence>,
}

#[derive(Debug, Clone, Default)]
pub struct SearchRequest {
    pub query: String,
    pub kind: Option<DiscoveryKind>,
    pub limit: usize,
}

#[async_trait]
pub trait DiscoveryIndex: Send + Sync {
    async fn search(&self, request: SearchRequest) -> anyhow::Result<Vec<SearchHit>>;
    async fn describe(&self, id: &str) -> anyhow::Result<Option<DiscoveryItem>>;
    async fn list(&self, kind: Option<DiscoveryKind>) -> anyhow::Result<Vec<DiscoveryItem>>;
    async fn home(&self) -> anyhow::Result<Value> {
        Ok(default_home())
    }
}

fn default_home() -> Value {
    json!({
        "resource": { "type": "gateway", "id": "home" },
        "result": {
            "status": "ready",
            "message": "Available workflows and proxy capabilities can be discovered here."
        },
        "links": [
            {
                "rel": "search",
                "title": "Search workflows and capabilities",
                "method": "praxec.query",
                "args": { "query": "" },
                "inputSchema": {
                    "type": "object",
                    "required": ["query"],
                    "properties": {
                        "query": { "type": "string" },
                        "kind": { "type": "string", "enum": ["workflow", "capability", "connection"] },
                        "limit": { "type": "integer", "default": 10 }
                    },
                    "additionalProperties": false
                }
            },
            {
                "rel": "list_workflows",
                "title": "List configured workflows",
                "method": "praxec.query",
                "args": { "query": "", "kind": "workflow" }
            },
            {
                "rel": "list_capabilities",
                "title": "List proxy capabilities",
                "method": "praxec.query",
                "args": { "query": "", "kind": "capability" }
            },
            {
                "rel": "observe",
                "title": "Replay the structured audit event stream (bounded window; the pull complement to `praxec observe --follow`)",
                "method": "praxec.query",
                "args": { "observe": true },
                "inputSchema": {
                    "type": "object",
                    "required": ["observe"],
                    "properties": {
                        "observe": { "type": "boolean" },
                        "since": { "type": "string", "description": "RFC3339 floor — only events with timestamp >= since" },
                        "limit": { "type": "integer", "default": 200 }
                    },
                    "additionalProperties": false
                }
            }
        ]
    })
}

/// In-memory lexical discovery index. Construct via
/// `InMemoryDiscoveryIndex::from_config(config)` to populate from the parsed
/// gateway YAML, or via `new(items)` if you're building documents yourself.
#[derive(Default, Clone)]
pub struct InMemoryDiscoveryIndex {
    docs: Arc<Vec<DiscoveryItem>>,
}

impl InMemoryDiscoveryIndex {
    pub fn new(items: Vec<DiscoveryItem>) -> Self {
        Self {
            docs: Arc::new(items),
        }
    }

    /// Build an index from a parsed gateway config. Fails (CMP-031) if
    /// `discovery.include` carries an unknown token, which would otherwise
    /// silently produce a partial index.
    pub fn from_config(config: &Value) -> anyhow::Result<Self> {
        Ok(Self::new(crate::discovery_indexer::index_from_config(
            config,
        )?))
    }

    pub fn extend(&mut self, items: impl IntoIterator<Item = DiscoveryItem>) {
        let mut owned =
            Arc::try_unwrap(std::mem::take(&mut self.docs)).unwrap_or_else(|arc| (*arc).clone());
        owned.extend(items);
        self.docs = Arc::new(owned);
    }

    pub fn len(&self) -> usize {
        self.docs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }
}

#[async_trait]
impl DiscoveryIndex for InMemoryDiscoveryIndex {
    async fn search(&self, request: SearchRequest) -> anyhow::Result<Vec<SearchHit>> {
        let limit = if request.limit == 0 {
            10
        } else {
            request.limit
        };
        let terms = tokenize(&request.query);
        let want_all = terms.is_empty();

        // Guidance fragments are looked up by known subject via
        // `gateway.describe` — they're not the answer to "what can I do?".
        // They stay in the index (so describe can find them) but are
        // excluded from search unless the caller asks for them explicitly
        // via `kind=guidance`.
        let mut hits: Vec<SearchHit> = self
            .docs
            .iter()
            .filter(|d| match request.kind {
                Some(k) => k == d.kind,
                None => d.kind != DiscoveryKind::Guidance,
            })
            .filter_map(|d| {
                let score = score_doc(d, &terms);
                if want_all || score > 0.0 {
                    Some(SearchHit {
                        score: if want_all { 1.0 } else { score },
                        item: d.clone(),
                        evidence: None,
                    })
                } else {
                    None
                }
            })
            .collect();

        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.item.id.cmp(&b.item.id))
        });
        hits.truncate(limit);
        Ok(hits)
    }

    async fn describe(&self, id: &str) -> anyhow::Result<Option<DiscoveryItem>> {
        Ok(self.docs.iter().find(|d| d.id == id).cloned())
    }

    async fn list(&self, kind: Option<DiscoveryKind>) -> anyhow::Result<Vec<DiscoveryItem>> {
        Ok(self
            .docs
            .iter()
            .filter(|d| match kind {
                Some(k) => k == d.kind,
                None => d.kind != DiscoveryKind::Guidance,
            })
            .cloned()
            .collect())
    }
}

// ---------- semantic (opt-in add-on) ----------------------------------------

/// The text embedded for a discovery item — title, id, description, tags,
/// aliases, and the builder-filled free text. The query is embedded the same
/// way so the vectors are comparable.
pub fn item_embed_text(item: &DiscoveryItem) -> String {
    let tags = item.tags.join(" ");
    let aliases = item.aliases.join(" ");
    [
        item.title.as_str(),
        item.id.as_str(),
        item.description.as_str(),
        tags.as_str(),
        aliases.as_str(),
        item.text.as_str(),
    ]
    .into_iter()
    .filter(|s| !s.is_empty())
    .collect::<Vec<_>>()
    .join(" ")
}

/// A semantic discovery index — the **opt-in add-on** activated only when a user
/// has registered (and pays for) an embedding model. It precomputes one
/// embedding per item at build time and ranks search by a hybrid of the lexical
/// score and query↔item cosine similarity, so natural-language intent surfaces
/// the right items even without keyword overlap. With no embedding model
/// configured the runtime stays on the free lexical [`InMemoryDiscoveryIndex`];
/// if a query can't be embedded at request time, this falls back to pure
/// lexical.
pub struct SemanticDiscoveryIndex {
    items: Arc<Vec<DiscoveryItem>>,
    /// Parallel to `items`; `None` for items whose embedding failed at build
    /// time (they fall back to the lexical score).
    embeddings: Arc<Vec<Option<Vec<f32>>>>,
    embedder: Arc<dyn crate::embeddings::EmbeddingProvider>,
}

impl SemanticDiscoveryIndex {
    /// Build by embedding every item's text once.  A per-item embed failure is
    /// non-fatal: the item is skipped (it remains searchable via the lexical
    /// index) and a warning is emitted.  Only items that embed successfully
    /// participate in semantic re-ranking; this keeps the index alive even
    /// under a partially-degraded embedding backend.
    ///
    /// Note: this is the BUILD path. The request-time query-embed in `search`
    /// intentionally degrades to lexical on error — that is a per-query best
    /// effort, not a structural defect, and is left untouched.
    pub async fn build(
        items: Vec<DiscoveryItem>,
        embedder: Arc<dyn crate::embeddings::EmbeddingProvider>,
    ) -> anyhow::Result<Self> {
        // Use Option per slot so that the embedding vec stays parallel to the
        // items vec; skipped items get None and fall back to lexical-only.
        let mut embeddings: Vec<Option<Vec<f32>>> = Vec::with_capacity(items.len());
        for item in &items {
            match embedder.embed(&item_embed_text(item)).await {
                Ok(vec) => embeddings.push(Some(vec)),
                Err(e) => {
                    tracing::warn!(
                        item_id = %item.id,
                        error = %e,
                        "failed to embed discovery item '{}': {e} — skipping (falls back to lexical)",
                        item.id
                    );
                    embeddings.push(None);
                }
            }
        }
        Ok(Self {
            items: Arc::new(items),
            embeddings: Arc::new(embeddings),
            embedder,
        })
    }

    fn kind_ok(item: &DiscoveryItem, kind: Option<DiscoveryKind>) -> bool {
        match kind {
            Some(k) => k == item.kind,
            None => item.kind != DiscoveryKind::Guidance,
        }
    }
}

#[async_trait]
impl DiscoveryIndex for SemanticDiscoveryIndex {
    async fn search(&self, request: SearchRequest) -> anyhow::Result<Vec<SearchHit>> {
        let limit = if request.limit == 0 {
            10
        } else {
            request.limit
        };
        let terms = tokenize(&request.query);
        let want_all = terms.is_empty();

        let candidates: Vec<usize> = (0..self.items.len())
            .filter(|&i| Self::kind_ok(&self.items[i], request.kind))
            .collect();

        // Lexical scores + their max (for normalisation against cosine).
        let lexical: Vec<f32> = candidates
            .iter()
            .map(|&i| score_doc(&self.items[i], &terms))
            .collect();
        let lex_max = lexical.iter().copied().fold(0.0_f32, f32::max);

        // Embed the query once; empty (no embedder output) → pure-lexical fallback.
        let qvec = if want_all {
            Vec::new()
        } else {
            self.embedder
                .embed(&request.query)
                .await
                .unwrap_or_default()
        };
        let semantic_on = !qvec.is_empty();

        let mut hits: Vec<SearchHit> = candidates
            .iter()
            .enumerate()
            .filter_map(|(k, &i)| {
                let lex = lexical[k];
                let score = if want_all {
                    1.0
                } else if semantic_on {
                    let lex_norm = if lex_max > 0.0 { lex / lex_max } else { 0.0 };
                    // If this item has no embedding (failed at build time), fall
                    // back to lexical-only for it.
                    let cos = self.embeddings[i]
                        .as_ref()
                        .map(|ev| crate::embeddings::cosine_similarity(&qvec, ev))
                        .unwrap_or(0.0);
                    0.5 * lex_norm + 0.5 * cos
                } else {
                    lex
                };
                if want_all || score > 0.0 {
                    Some(SearchHit {
                        score,
                        item: self.items[i].clone(),
                        evidence: None,
                    })
                } else {
                    None
                }
            })
            .collect();

        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.item.id.cmp(&b.item.id))
        });
        hits.truncate(limit);
        Ok(hits)
    }

    async fn describe(&self, id: &str) -> anyhow::Result<Option<DiscoveryItem>> {
        Ok(self.items.iter().find(|d| d.id == id).cloned())
    }

    async fn list(&self, kind: Option<DiscoveryKind>) -> anyhow::Result<Vec<DiscoveryItem>> {
        Ok(self
            .items
            .iter()
            .filter(|d| Self::kind_ok(d, kind))
            .cloned()
            .collect())
    }
}

// ---------- scoring ---------------------------------------------------------

fn tokenize(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect()
}

fn score_doc(doc: &DiscoveryItem, terms: &[String]) -> f32 {
    let title = doc.title.to_lowercase();
    let id = doc.id.to_lowercase();
    let desc = doc.description.to_lowercase();
    let text = doc.text.to_lowercase();
    let tags = doc.tags.join(" ").to_lowercase();
    let aliases = doc.aliases.join(" ").to_lowercase();

    terms.iter().fold(0.0_f32, |acc, term| {
        acc + field_score(&title, term, 6.0)
            + field_score(&id, term, 5.0)
            + field_score(&tags, term, 3.0)
            + field_score(&aliases, term, 3.0)
            + field_score(&desc, term, 2.0)
            + field_score(&text, term, 1.0)
    })
}

fn field_score(field: &str, term: &str, weight: f32) -> f32 {
    if field.contains(term) {
        return weight;
    }

    let words: Vec<&str> = field
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();

    if term.len() >= 2 && words.iter().any(|w| w.starts_with(term)) {
        return weight * 0.7;
    }

    if term.len() >= 4 {
        let best = words
            .iter()
            .map(|w| trigram_similarity(term, w))
            .fold(0.0_f32, f32::max);
        if best > 0.3 {
            return weight * best * 0.5;
        }
    }

    0.0
}

fn trigram_similarity(a: &str, b: &str) -> f32 {
    let ta = trigrams(a);
    let tb = trigrams(b);
    if ta.is_empty() && tb.is_empty() {
        return 1.0;
    }
    if ta.is_empty() || tb.is_empty() {
        return 0.0;
    }
    let intersection = ta.iter().filter(|t| tb.contains(t)).count();
    let union = ta.len() + tb.len() - intersection;
    if union == 0 {
        0.0
    } else {
        intersection as f32 / union as f32
    }
}

fn trigrams(s: &str) -> Vec<[u8; 3]> {
    let bytes = s.as_bytes();
    if bytes.len() < 3 {
        return vec![];
    }
    let mut out = Vec::with_capacity(bytes.len() - 2);
    for i in 0..bytes.len() - 2 {
        out.push([bytes[i], bytes[i + 1], bytes[i + 2]]);
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod semantic_tests {
    use super::*;
    use crate::embeddings::{EmbeddingError, EmbeddingProvider};

    /// A deterministic 2-axis fake: axis 0 fires on speed/cache words, axis 1 on
    /// auth/identity words — so intent ("speed") maps to a model whose text says
    /// "cache", without any shared keyword.
    struct FakeEmbedder;

    #[async_trait]
    impl EmbeddingProvider for FakeEmbedder {
        async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
            let t = text.to_lowercase();
            let speed = ["speed", "fast", "cache", "latency", "perf"]
                .iter()
                .any(|w| t.contains(w)) as i32 as f32;
            let auth = ["auth", "login", "identity", "credential"]
                .iter()
                .any(|w| t.contains(w)) as i32 as f32;
            Ok(vec![speed, auth])
        }
        async fn health_check(&self) -> Result<(), EmbeddingError> {
            Ok(())
        }
        fn dimensions(&self) -> usize {
            2
        }
        fn backend_name(&self) -> &'static str {
            "fake"
        }
    }

    /// Always errors — exercises the H4 build-time fail-fast path.
    struct FailingEmbedder;

    #[async_trait]
    impl EmbeddingProvider for FailingEmbedder {
        async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbeddingError> {
            Err(EmbeddingError::BackendFailed("backend down".into()))
        }
        async fn health_check(&self) -> Result<(), EmbeddingError> {
            Err(EmbeddingError::HealthCheckFailed("backend down".into()))
        }
        fn dimensions(&self) -> usize {
            2
        }
        fn backend_name(&self) -> &'static str {
            "failing"
        }
    }

    /// Embedder that fails only for the item whose text contains "bad".
    struct SelectiveFailEmbedder;

    #[async_trait]
    impl EmbeddingProvider for SelectiveFailEmbedder {
        async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
            if text.contains("bad") {
                Err(EmbeddingError::BackendFailed("transient error".into()))
            } else {
                Ok(vec![1.0, 0.0])
            }
        }
        /// Healthy overall — only the one poisoned item fails.
        async fn health_check(&self) -> Result<(), EmbeddingError> {
            Ok(())
        }
        fn dimensions(&self) -> usize {
            2
        }
        fn backend_name(&self) -> &'static str {
            "selective-fail"
        }
    }

    #[tokio::test]
    async fn build_skips_item_and_warns_on_per_item_embed_error() {
        // Resilience: a per-item embed failure must NOT abort the whole build.
        // The bad item is skipped (falls back to lexical); good items are indexed.
        let items = vec![
            item("good", "Good item", "this is fine"),
            item("bad-item", "Bad item", "bad text that triggers a failure"),
        ];
        let idx = SemanticDiscoveryIndex::build(items, Arc::new(SelectiveFailEmbedder))
            .await
            .expect("build must succeed even when one item fails to embed");
        // The good item is still searchable
        let hits = idx
            .search(SearchRequest {
                query: "good".into(),
                kind: None,
                limit: 10,
            })
            .await
            .unwrap();
        assert!(
            hits.iter().any(|h| h.item.id == "good"),
            "good item must appear in search results"
        );
    }

    #[tokio::test]
    async fn build_with_all_failing_embeds_still_succeeds() {
        // Even if every item fails to embed, the build must succeed
        // (returning an index that uses pure lexical search).
        let items = vec![item("optimize", "Optimize", "add a cache layer")];
        SemanticDiscoveryIndex::build(items, Arc::new(FailingEmbedder))
            .await
            .expect("build must succeed even when all items fail to embed");
    }

    fn item(id: &str, title: &str, desc: &str) -> DiscoveryItem {
        DiscoveryItem {
            id: id.into(),
            kind: DiscoveryKind::Workflow,
            title: title.into(),
            description: desc.into(),
            tags: vec![],
            examples: vec![],
            aliases: vec![],
            text: String::new(),
            links: vec![],
            verb: None,
            body: None,
            source: None,
        }
    }

    #[tokio::test]
    async fn surfaces_intent_without_keyword_overlap() {
        let items = vec![
            item("optimize", "Optimize", "add a cache layer to responses"),
            item("login", "Login", "auth and identity"),
        ];
        let idx = SemanticDiscoveryIndex::build(items, Arc::new(FakeEmbedder))
            .await
            .unwrap();

        // "speed" appears in neither item's text — lexical alone finds nothing,
        // but semantically it matches the cache item.
        let hits = idx
            .search(SearchRequest {
                query: "speed".into(),
                kind: None,
                limit: 10,
            })
            .await
            .unwrap();
        assert!(
            !hits.is_empty(),
            "semantic add-on should surface the cache item"
        );
        assert_eq!(hits[0].item.id, "optimize");
    }

    #[tokio::test]
    async fn empty_query_returns_all() {
        let items = vec![item("a", "A", "x"), item("b", "B", "y")];
        let idx = SemanticDiscoveryIndex::build(items, Arc::new(FakeEmbedder))
            .await
            .unwrap();
        let hits = idx
            .search(SearchRequest {
                query: String::new(),
                kind: None,
                limit: 10,
            })
            .await
            .unwrap();
        assert_eq!(hits.len(), 2);
    }
}
