# model_catalog.json — the model catalog (core)

The single source of truth for the models praxec knows about and **what they're
good at**. Capability is a property of the *model*, so the catalog lives in core
(`core::model_catalog`) and is consumed by **both** the cockpit's model picker and
a `kind: llm` step's `needs:` resolution (the suggestor ranks this catalog by
`affinity_fit`).

The numbers (intelligence, speed, price, affinity scores) are **data, sourced and
dated** — not Rust consts and not guesses. The file carries its own provenance
(`source`, `captured`) and is refreshed by editing data, never code (the "data in
config, not code" principle).

## Schema

```jsonc
{
  "source":   "where each axis came from",
  "captured": "YYYY-MM-DD",
  "models": [
    {
      "vendor": "openrouter",            // == ProviderId slug; the SDK the chat runs on
      "model":  "google/gemini-3.1-pro", // the model id passed to the provider
      "input_usd_per_million":  2.0,
      "output_usd_per_million": 12.0,
      "context": 1000000,
      "intelligence": 57,                // capability index (higher = more capable)
      "speed_tps": 130,                  // output tokens/sec (the "time" axis)
      "tools": true,                     // MUST call tools to be a conductor (hard filter)
      "reasoning_levels": ["none","low","medium","high"], // rig effort levels supported
      "scores": {                        // AFFINITY scores — what it's GOOD AT
        "coding": 55, "agentic": 54, "reasoning": 61
      }
    }
  ]
}
```

## What models are good at (`scores`) — affinity scores

The vocabulary is centralized on **`Affinity`** (core `model_resolver`): `coding`,
`reasoning` (AA's "scientific reasoning" — math/science live here), `agentic`,
plus `prose` / `web-search` / `recon`. A model's `scores` are its **affinity
scores** (the AA sub-indices, whose 25%-each mean — with `general` — is the
overall `intelligence`). `general` isn't stored; it *is* the overall.

These let the **suggestor** match a *task* to the right model: a `kind: llm` step
declares **`needs: [coding, agentic]`** and the suggestor ranks candidates by
`affinity_fit` — a weighted blend of overall intelligence **and** the needed
affinity scores (`core::model_resolver::{Affinity, AffinityScores, affinity_fit}`,
surfaced in the cockpit as `recommend_chat_for_affinities`).

- The cockpit conductor is an **agentic** job, so its gate ranks on `agentic`.
- An unscored affinity falls back to overall `intelligence` (partial data ranks).
- **Provenance caveat:** the *overall* `intelligence` is pinned to AA; the
  affinity `scores` are **interpolated from each model's published profile around
  its index** and should be pinned to AA's per-category leaderboards
  (`artificialanalysis.ai/models/capabilities/{coding,agents,…}`) on refresh. The
  coding specialist (`qwen/qwen3-coder`) and open generalists (DeepSeek V4, Kimi
  K2.6, GLM-5.1, MiniMax M3) are included so there's real open-source +
  task-specialist choice, not only frontier models.

## Sources (captured 2026-06-10; openrouter additions 2026-06-23)

> **2026-06-23 additions.** Five `openrouter` entries were added —
> `z-ai/glm-5.2`, `deepseek/deepseek-v4-pro`, `deepseek/deepseek-v4-flash`,
> `deepseek/deepseek-r1-0528`, `z-ai/glm-4.7-flash`. Their **price is sourced**
> from OpenRouter (captured 2026-06-23); their **intelligence/affinity scores are
> best-available ESTIMATES** calibrated relative to existing entries, pending an
> Artificial-Analysis refresh.


- **intelligence** — [Artificial Analysis Intelligence Index](https://artificialanalysis.ai/evaluations/artificial-analysis-intelligence-index).
  Pinned for the models AA scores directly (Opus 4.8 61.4, GPT-5.5 60.2,
  Gemini 3.1 Pro 57, DeepSeek V4 52). Sonnet 4.6, Haiku 4.5 and Gemini 3.5 Flash
  are not separately on the index snapshot used; their values are interpolated
  from the same scale and should be refreshed against AA when published.
- **speed_tps** — Artificial Analysis output-speed figures (representative
  sustained tok/s; providers vary by region/load).
- **price** — providers' published API pricing pages. Gemini 3.1 Pro = $2/$12.
  Flash and DeepSeek pricing vary by tier/variant; values here are the standard
  published tier.

## Refreshing

Edit this file (or ship an override — see below) and bump `captured`. No rebuild
of the cockpit is required to change the data; only adding a *new field* touches
code. Don't gate a Praxec release on new models hitting the market.

### Override precedence (shared `core::catalog`)

1. `$PRAXEC_MODEL_CATALOG_FILE`
2. `<config-dir>/praxec/model_catalog.json`
3. `./.praxec/model_catalog.json`
4. this shipped default

## Licensing note

The Artificial Analysis Intelligence Index is AA's methodology; we cite it as the
source for the capability axis rather than redistributing their dataset. Pricing
is each provider's published rate (a fact). If shipping AA-derived numbers as the
default ever conflicts with their terms, prefer an open capability source (e.g.
the HF leaderboard) for the shipped default and let AA be a user-side override.
