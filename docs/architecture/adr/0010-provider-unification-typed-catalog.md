# ADR-0010: Provider identity is one typed catalog

**Status:** Accepted

**Date:** 2026-06-05

## Context

Praxec named LLM providers in three places that had silently drifted: the TUI
key store (`provider_keys.rs::ProviderId`), the `kind: llm` factory
(`provider_factory.rs::DefaultProviderFactory`), and the core agent resolver
(`agent_resolver::Provider`). The authoritative token set is aether-llm's
`ModelProviderParser` — it parses the `provider:model` string the agent path
hands the runtime — and cross-referencing it surfaced real bugs: the resolver
emitted `google` while aether registers only `gemini` (so a Gemini `delegate:`
failed at spawn), and `lmstudio` was not an aether token at all.

## Decision

One typed source of truth for the **curated** provider set, projected to all
surfaces by **exhaustive `match`** so a missing provider is a *compile error*,
not silent drift. A new module `crates/praxec-core/src/providers.rs`
defines a closed `ProviderId` enum and a single `descriptor()` match returning a
`ProviderDescriptor { slug, display, credentials, availability }`, where each
`slug` equals the aether parser token. Canonical slugs reconcile the drift
(`Gemini` replaces `Google`; `Openrouter`/`Llamacpp` join the resolver;
`Bedrock` is gated on the `bedrock` cargo feature). The open-ended
OpenAI-compatible long tail stays declarative via `Custom { endpoint }` (no
recompile); **model IDs stay free-form** — the catalog carries providers only. A
boundary test guards the praxec↔aether seam so neither side drifts unnoticed
on a version bump.

A closed enum is the right tool because adding a provider is rare (~1–3/yr) and,
for bespoke-protocol providers, requires new compiled code regardless — the enum
adds no cost not already being paid, while `Custom` serves the config-only tail.

## Consequences

- Provider drift becomes impossible: adding a `ProviderId` variant fails to
  compile until its descriptor arm exists, and the boundary test fails if the
  aether token set moves.
- The Gemini/`google` and `lmstudio` spawn bugs are fixed at the source.
- The cost/pricing catalog (`cost.rs`) and aether's `deepseek`/`moonshot`/`zai`
  tokens are explicitly out of scope (reconcile-only; add later as one-line
  catalog entries).

## Status update (2026-07)

The `agent_resolver::Provider` named in **Context** has since been renamed
`model_resolver::Provider` (`crates/praxec-core/src/model_resolver.rs`); the
`Custom { endpoint }` long-tail variant now lives there as
`model_resolver::Provider::Custom`. The historical narrative above is left as
written — this note only points the current reader at the present module name.

## References

- Module: `crates/praxec-core/src/providers.rs`
- Provider resolver (renamed from `agent_resolver`):
  `crates/praxec-core/src/model_resolver.rs`
- Authoritative seam: aether-llm `ModelProviderParser::default`
