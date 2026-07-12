# Contributing to praxec

Thanks for your interest in helping. This project is small, opinionated,
and moves deliberately — please read this page before opening a PR, so
your time and ours land in the same place.

## Quick links

- **Code of Conduct:** [`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md)
- **Security disclosures:** [`SECURITY.md`](SECURITY.md) — **do not file
  a public issue for security bugs**.
- **Development setup, workspace layout, common commands:**
  [`docs/development/internals.md`](docs/development/internals.md) — the authoritative
  reference; this page does not duplicate it.
- **Stability / what we promise not to break:**
  [`docs/reference/stability.md`](docs/reference/stability.md).
- **Verification coverage (what's tested vs. what isn't):**
  see the "Verification coverage" section in [`docs/reference/stability.md`](docs/reference/stability.md).

## Branching model

We use a gitflow-style flow, enforced by branch rulesets and CI:

```
feature/*  ->  dev  ->  main
```

- **`feature/*` (or `fix/*`)** — where you do your work. Branch off `dev`.
  Commit in logical groups; a single deliverable is one branch, not a
  stack of PRs.
- **`dev`** — the integration branch and the repo default. Feature work
  merges here via PR. This is where things come together before release.
- **`main`** — the release branch. It only ever advances by merging
  `dev`. A CI guard (`main only accepts merges from dev`) rejects any PR
  into `main` whose source branch isn't `dev`.

Both `dev` and `main` are protected: **direct pushes are blocked, every
change goes through a pull request, and CI (`build + test + lint`) must be
green before merge — for everyone, including maintainers.** There is no
admin bypass, so plan on the PR round-trip even for small changes.

## Before you open a PR

1. **Open an issue first** for anything larger than a bug fix or doc
   tweak. Coordination is cheaper than rework.
2. **Run the same checks CI runs**, locally:
   ```bash
   cargo fmt --all -- --check
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace
   ```
3. **Test coverage.** All behavioural changes need atomic, declarative
   tests — one observable property per test, named after the property.
   See existing tests in `crates/*/tests/` for the patterns.
4. **CHANGELOG.** Add a bullet under `## [Unreleased]` describing the
   change. The release-guard CI step enforces this when a version
   bumps; the project convention is to update CHANGELOG with the
   change PR, not the release PR.

## What we welcome

- **Bug fixes**, with a regression test.
- **New executor kinds** behind the existing `Executor` trait.
- **New audit sinks** (OTLP, file rotation, SIEM-specific) behind the
  existing `AuditSink` trait.
- **New store backends** (Redis, DynamoDB) behind the existing
  `Store` trait.
- **Documentation** — especially worked examples, troubleshooting,
  and "I tried this and it didn't work" patterns.
- **Stress tests.** See [`docs/development/stress-tests.md`](docs/development/stress-tests.md)
  for the format and how to add one.

## What we're cautious about

- **Surface-area additions to the two-tool MCP surface (`praxec.query`
  + `praxec.command`) or YAML top-level keys.** These are Tier 1 stable;
  changes need a strong
  motivating example and usually involve a deprecation cycle.
- **New configuration knobs** when an existing knob composes to the
  same effect. We try to make the YAML small and orthogonal.
- **Dependencies.** Each new transitive dep is an attack surface and a
  maintenance cost. Prefer narrow, well-maintained crates.

## Style

- Rust 2021, formatted with `rustfmt` defaults.
- Comments only where the *why* is non-obvious; don't restate what the
  code says.
- Public APIs are doc-commented; internal items usually aren't.
- Tests state intent in their names: prefer
  `fn workflow_submit_rejects_stale_version` over `fn test_submit_1`.

## Sign-off

By submitting a PR you certify that your contribution is your own work
and that you have the right to license it under Apache-2.0. We use
the [Developer Certificate of Origin](https://developercertificate.org/) —
sign commits with `git commit -s`.

## Nightly CI secrets

`.github/workflows/nightly.yml` requires the following repository secrets:

- `ANTHROPIC_API_KEY_CI` — a CI-scoped API key. Spend cap recommended at $5/month; the nightly's smoke-ete walk uses ~$0.10–$0.50 per run.
- `OPENAI_API_KEY_CI` — same shape.
- `GOOGLE_API_KEY_CI` — same shape.

Set these in repo Settings → Secrets and variables → Actions. The nightly workflow is gated to the canonical repo (`if: github.repository == 'praxec/praxec'`) so forks don't accidentally trigger live API calls.

## Maintainer expectations

We aim for:

- Acknowledgement of a PR within **5 business days**.
- A review (accept / request changes / close with reason) within
  **10 business days**.

If you don't hear back, please ping the thread — it slipped, not
ignored.
