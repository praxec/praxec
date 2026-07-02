# Design: Semantic Catalog & Guided Settings

**Status:** Draft for review — 2026-06-10
**Scope:** praxec-core (embedding activation + discovery upgrade) + a reusable
picker surface in the cockpit/TUI. Design-big / build-small: this is the full
surface and approach; we apply it incrementally.

## Context & thesis

Today you configure praxec by typing slugs (`anthropic:claude-opus-4-8`),
hand-editing `models.yaml`, and knowing which OpenRouter model is good. The
discovery the LLM uses to find tools/capabilities is **lexical** (keyword/trigram).
We want the inverse: **state intent, get ranked options with their tradeoffs.**

One embedding model, configured once, indexes everything praxec can offer —
capabilities, tools, skills, scripts, sub-workflows, **and** providers/models — so
any principal (the chat LLM, an execution-step LLM, or a human) says what it wants
and the right things surface, ranked by relevance, effectiveness, and cost.
Choosing a setting (the chat model, a per-affinity execution model, an agent's
tools) becomes the **same** "describe → search → filter → pick" interaction
everywhere. This is praxec's harness inversion applied to configuration.

## What already exists (grounding)

- `core::embeddings`: `EmbeddingProvider` trait, `HttpEmbedder` (Ollama +
  OpenAI-compatible), `cosine_similarity`, `entry_embed_text`, a working semantic
  ranker (`lexicon_candidates` Tier 3). **Built and tested but dormant** — the
  serve path always uses `NoopEmbedder`; `parse_embeddings_config` is test-only.
- `core::discovery`: `DiscoveryItem { id, kind, title, description, tags, … }` +
  `DiscoveryIndex::search` — **lexical only**, no vectors.
- `core::model_resolver`: `Affinity {coding, reasoning, prose, web-search, recon}`
  × `Tier {frontier, standard, commoditized}`, bound in `models.yaml`.
- `llm-executor::cost`: a 16-entry hand-verified `$/Mtok` catalog (provenance +
  `verified_at` + staleness), already enforcing budget caps.
- `aether-llm` catalog: `LlmModel::all()` / `available_models()` — ~100+ models
  with provider, display name, context window, reasoning/modality flags, and
  `required_env_var()` (so we can show "models you have a key for"). **No cost in
  its Rust API; no embeddings** (chat-only).

So: the embedding engine is mostly **activation**, capability metadata is **free
from aether-llm**, and the new work is (a) a unified embedded index, (b) the
benchmark/effectiveness + cost layers praxec owns, (c) the picker surface.

## 1. The unified catalog (data model)

One index of `CatalogEntry`, superseding the lexical discovery index by extending
it with vectors and adding provider/model kinds:

```
CatalogEntry {
  kind: Provider | Model | Tool | Skill | Script | Workflow | Capability
  id, name, description            // description is the embedded text
  embedding: { model_id, dims, vector }   // tags the embedder used → invalidation
  metadata: kind-specific, structured for hard filtering:
    Model  → vendor, model_id, context_window, modalities, reasoning,
             availability {key_present | local}, cost {…}, effectiveness {…}
    Tool/… → today's DiscoveryItem fields (tags, examples, aliases, links, …)
}
```

One index, **kind-filtered** per context (a tool search never returns models).
`cost` and `effectiveness` are their own declarative layers (§4, §5), joined onto
Model entries by `vendor:model_id`.

## 2. The shared embedding layer

- One `EmbeddingProvider`, configured **once** and wired into the serve path, the
  cockpit, and discovery (today dormant). Reuses the existing `HttpEmbedder`.
- **Embeddings come through `rig` (rig-core), not raw HTTP.** Decision: point the
  embedding layer at `rig`, which treats embeddings as first-class. praxec's
  `EmbeddingProvider` becomes a thin adapter over rig's `EmbeddingModel`
  (`embed_text`/`embed_texts`, async; vectors are `Vec<f64>`), built per provider
  (`openai::Client::from_env().embedding_model("text-embedding-3-small")`).
  Providers with embeddings in rig-core: OpenAI, Gemini, Cohere, Voyage, Mistral,
  Together, **OpenRouter**, Ollama (explicit dims) — so OpenRouter embeddings *are*
  available, through the library. **rig also supplies the index + search**
  (`InMemoryVectorStore`, brute-force or LSH; `VectorStoreIndex::top_n`; cosine),
  with a path to real stores (Qdrant/LanceDB/pgvector) — we reuse that instead of
  bespoke cosine/inline storage. aether-llm stays the chat/wisp engine; the
  raw-HTTP `HttpEmbedder` is retired.
- **Same model everywhere is required, not just tidy:** vectors from different
  models live in different spaces/dims and aren't comparable. The embedder's
  `{model_id, dims}` is stamped on every entry; switching it invalidates the index
  and triggers a **reindex** (re-embed all entries).
- Storage/search: reuse rig's `InMemoryVectorStore` + `VectorStoreIndex`
  (brute-force now; LSH or an external store — Qdrant/pgvector — when the catalog
  grows). This replaces praxec's bespoke cosine + inline `_embedding` storage.

## 3. Effectiveness via trusted benchmarks (your answer #1)

Effectiveness is **not invented and not LLM-judged**; it is sourced, attributed,
and overridable. A declarative **benchmark catalog** (shipped snapshot + user
override; **no scraping** — refresh is a declarative file/API update):

```
benchmark_source {
  id, name, url, captured_at,
  trust_tier: trusted | provisional | ignored   // user-assignable
}
benchmark_score {
  model: "vendor:model_id",
  source: <source id>,
  metric: <e.g. swe-bench, gpqa, arena-elo, aider-polyglot>,
  value, captured_at
}
```

- **Metric → Affinity mapping** (praxec owns): e.g. SWE-bench/Aider → `coding`;
  GPQA/MMLU-Pro → `reasoning`; arena writing/creative → `prose`; etc.
- **Effectiveness(model, affinity)** = trust-weighted, normalized aggregate of the
  mapped scores. Derived, inspectable (you can see which sources/metrics fed it).
- **Manual layer wins:** assign/adjust source **trust tiers**; **override** a
  model's effectiveness for an affinity; **edit/add** entries. Overrides carry
  provenance `manual` and beat sourced data.
- `Tier {frontier|standard|commoditized}` becomes a **derived bucket** of the
  effectiveness score (auto), with a manual pin available — feeding the existing
  `models.yaml` affinity×tier resolver directly.

**Sources (grounded).** No single feed is complete — which is exactly why the
multi-source + trust-tier + manual-override model above is the right shape:
- **HF Open LLM Leaderboard** — the openly-licensed, machine-readable *anchor*:
  the Results dataset (JSON/Parquet) carries MMLU-Pro, GPQA, MuSR, MATH, IFEval,
  BBH; the Requests dataset carries params/arch/precision. Maps to `reasoning`
  (MMLU-Pro/GPQA/MuSR/BBH/MATH) and instruction-following (IFEval). **Caveats:** it
  covers **open-weight models only** (Claude/GPT/Gemini aren't run in the harness),
  has **no coding benchmark**, and was **archived/frozen (~early 2025)** — so it's a
  static snapshot of the *open* tier, not a live closed-frontier feed. (Confirm
  current status at ingest.)
- **Closed-frontier + coding gap** → complementary sources, each ingested as a
  dated snapshot with its own trust tier: Artificial Analysis (Intelligence Index +
  coding/reasoning — rich, but **no documented public API / unclear reuse license**
  → likely manual snapshot), LMArena/Chatbot Arena Elo (community, includes closed),
  SWE-bench / Aider / BigCodeBench (→ `coding`), the Vellum leaderboard, and the HF
  *Big Benchmarks Collection* as a source registry.
- **No Rust crate (rig included) ships this** — we ingest the JSON/Parquet directly.

## 4. Cost

Make `cost.rs` **declarative** (the already-noted follow-up): per model
`{input, output, cache} $/Mtok` + provenance + `verified_at` + staleness; shipped
snapshot + user override; optional in-response/API refresh; **no scraping**. Reused
for the cost axis in the picker *and* the existing budget-cap enforcement.

**Source (grounded):** open, machine-readable, no-auth catalogs — **models.dev**
(`api.json` / `models.json` / `catalog.json`: pricing + context + modalities +
**embedding models** with dims) and/or **LiteLLM's
`model_prices_and_context_window.json`** (~200+ models; embedding entries carry
`mode: "embedding"` + `output_vector_size`). Either also supplies the
**embedding-model option list for the §8 bootstrap** and the Model entries for the
catalog (§1). Ship a dated snapshot + user override.

## 5. Search & ranking (deterministic, inspectable)

1. **Hard filter** — kind, vendor, availability (have-key/local), cost ceiling,
   min-effectiveness. (Drops the impossible.)
2. **Semantic rank** — cosine of the intent vs. entry descriptions. (Relevance.)
3. **Tradeoff (models)** — show effectiveness (per the relevant affinity) and cost
   side by side; never hide the tradeoff in one opaque number. Sort modes:
   **best effectiveness**, **cheapest**, **best value** (effectiveness/$). 

No LLM in the ranking path — it's a tool, per "build a tool, don't trust prompts."

## 6. The guided settings surface (the reusable picker)

One `CatalogPicker` component: **intent box + filter chips → ranked rows with
badges** (vendor · cost · effectiveness@affinity · context · availability) →
select. Reused everywhere a thing is chosen, with only the kind-filter + the write
target changing:

| Where | kind | writes |
|---|---|---|
| Chat-model gate (Mission Control LLM) | Model | the chat model (ADR-0005) |
| `models.yaml` affinity×tier slots (flow.configure-models) | Model | a binding |
| Per-step executor `model:`/`affinity:` | Model | workflow YAML |
| Agent `tools:` | Tool | agent config |
| Tool/skill/script discovery (LLM "what can I use?") | Tool/… | — (read) |
| Embedding-model bootstrap | (constrained list) | gateway embeddings config |

Same interaction, same component, consistent across all settings — the goal.

## 7. Where settings live (read catalog, write bindings)

- **Data (read):** catalog/benchmark/cost as declarative files — shipped defaults +
  user override in `~/.praxec/`, project override in `.praxec/`.
- **Bindings (write):** reuse existing writers — `core::provider_keys` (keys),
  `ModelsFile`/`models.yaml` (affinity×tier), `gateway.yaml` (embeddings backend),
  `agents.yaml`. One writer per binding type; the picker just calls them.

## 8. Bootstrap order (chicken-and-egg)

1. **Pick the embedding model — a startup gate, no hard-coded default.** On app
   startup, if no embedding model is configured, walk the user through
   **provider → embedding-model options** (a *structured* pick; you can't
   semantically search for the engine of semantic search). Options come from a
   machine-readable catalog (§3/§5), filtered by which keys are present — not a
   baked-in constant. Note: Ollama-local is **not** free — the weights are a real
   on-disk cost and space may not exist; an API model (e.g. OpenAI
   `text-embedding-3-small`, or a cheap OpenRouter embedding model) is often the
   better first choice. The picker presents cost + dims per option; the user chooses.
2. **Index** — embed all catalog entries (and reindex on change).
3. **Everything else** uses semantic search + filters.

## 9. Build-small increments (from this design)

1. **Activate the embedder** + the bootstrap embedding-model setting (wire the
   dormant infra into serve + cockpit).
2. **Unified catalog + embedded index** — `CatalogEntry`, search (filter → cosine),
   seeded with Models (from aether-llm `available_models()`).
3. **Cost layer** declarative; **effectiveness layer** (benchmark catalog + trust
   tiers + manual override + affinity mapping).
4. **`CatalogPicker`** component; apply to the **chat-model gate first**.
5. **Upgrade discovery** (tools/skills/scripts/sub-workflows) from lexical to
   embedded — "say what you want → right tools."
6. **Apply the picker** to the all-settings surface (flow.configure-models-style).

## 10. Open decisions (need sign-off)

1. **Benchmark starter sources + metric→affinity mapping** — confirm the set above
   (or substitute sources you trust) and the mapping.
2. **Refresh mechanism** — shipped snapshot + manual edits for v1, with a declarative
   update file later (vs. wiring an API fetch now)?
3. **Tier** — auto-derive `frontier/standard/commoditized` from effectiveness (with
   manual pin), or keep it purely manual?
4. **Index home** — extend `core::discovery` in place to be embedding-aware, or a
   new `core::catalog` that discovery folds into? (Leaning: one unified index.)
5. **RESOLVED — `rig` for embeddings now; execution migration later as a clean
   cutover.** Parity spike (rig-core 0.38.2) is **green** for the governed §33
   executor: streaming with typed tool-call + reasoning deltas (`ToolCall`/`ToolCallDelta`,
   `Reasoning`/`ReasoningDelta`), `Usage` incl. cache + reasoning tokens, structured
   `Reasoning {Text|Encrypted|Redacted|Summary}` (covers extended-thinking replay),
   Anthropic prompt caching (rich; `CacheControl` + TTL + budgeting), 20+ providers
   incl. anthropic/openai/gemini/openrouter/ollama. **Caveats:** OpenAI
   `reasoning_effort` and OpenAI/Gemini cache-*write* go via `additional_params`;
   **Bedrock/Vertex are separate companion crates** (`rig-bedrock`/`rig-vertexai`),
   not core. So a full execution move is viable as a **clean cutover** — moderate
   effort: re-derive the FMECA validation chain on rig's event/types + Bedrock via
   the companion crate — decided as its own step once we've built on rig.

## Grounding (files)

`core/src/embeddings.rs` · `core/src/discovery/` · `core/src/lexicon/lexicon_candidates.rs`
· `core/src/model_resolver/config.rs` (Affinity/Tier) · `llm-executor/src/cost.rs`
· aether-llm `catalog` (`LlmModel::all`/`available_models`) · `schemas/gateway-config.schema.json`
(`embeddings:` block) · fixtures `flow.configure-models` + `cap.research.model-inventory`.
