# Onboarding hardening — fix all 7 field-report defects (no lowered gates, no lost features)

> Apply praxec's own "correctness-in-structure" thesis to the CONFIG surface. Every change is ADDITIVE
> validation or new observability — never a relaxed gate, never a removed capability. Branch:
> `feat/onboarding-hardening` (off `dev`).

## Honest constraints (per goal)
- A config with NO `kind:agent`/affinity steps needs no `models_yaml` — must NOT newly error.
- An `affinity` that resolves via the `default:` chain is bound — must NOT be flagged unbound.
- D6 suppression only when a manifest EXPLICITLY opts in — never blanket-silence real warnings.
- Keep every existing error/warning; only ADD.

## The unifying design (operator input): the requirement TRAVELS, the key stays LOCAL
The root of D1/D7/affinity-fallback/currency is that a pack's model NEED dies at the pack boundary.
Fix: **packs declare their affinity requirements + a recommendation; the operator's models.yaml stays
the authoritative binding (keys/cost); the gap is loud + one-command-fixable.** Keep the affinity
indirection (portability). Add:
- **`praxec.repo.yaml` optional `affinities:` block** (travels with the pack): per affinity used by the
  pack — `{ tier: frontier|commodity|…, capability: "<one line>", recommended: <provider/model-id> }`.
- The keystone below reads it: an unbound affinity a MOUNTED pack uses → surfaces the pack's
  recommendation + the exact models.yaml snippet, not a silent default-fallback.
- **`praxec models bind <affinity>` + `doctor --fix`**: write the recommended binding into the
  operator's models.yaml using their EXISTING provider env (self-wiring on pull). Never overwrites an
  existing binding; never fabricates a key.
- Per-step model pin stays available (override for "only this model works"), but the pack RECOMMENDS
  (swappable) rather than PINS (hard) so portability holds.

## Cluster 1 — CONFIG READINESS + IT TRAVELS (keystone + pack-affinities + wiring + D1 + D7). Files: `praxec.repo.yaml` model + `repo.rs`, `preflight.rs`, `gateway.rs` (check/doctor/init/models-bind).
- **Keystone (agent-readiness invariant), in `check()` AND the doctor preflight:** for every MOUNTED
  definition that has a `kind:agent` step OR an `affinity:` on a step, assert the resolution chain:
  (a) if it uses an affinity, that affinity resolves to a binding in the in-force `models_yaml` OR the
  `default:` chain — an affinity with NO activity/override entry AND no default is `AFFINITY_UNBOUND`
  (loud, not silent commodity-fallback); (b) `doctor` then runs the existing per-binding credential
  preflight for the resolved model. Emit specific, model-error-quality messages. This is what turns
  "0 errors but nothing runs" loud. NO agent steps ⇒ no readiness requirement (no false error).
- **D1 — declared-but-unreadable `gateway.models_yaml` = hard error** in BOTH `check` and `doctor`:
  match the existing malformed-case (`MODELS_YAML_LOAD_FAILED`, same message quality: names file +
  consequence + fix). Key present + file unreadable/unparseable ⇒ error + non-zero exit. Key absent ⇒
  only the keystone applies. (Today: runtime WARN only — gateway_config.rs:900-906.)
- **D7 — `init` ends in `doctor`:** after scaffolding (`init` already writes models.yaml + wires
  `gateway.models_yaml`, gateway.rs:3507 / init.rs:60), RUN `doctor` on the result and print it, so a
  fresh install terminates in a readiness verdict (catches D1/D4/D7 at first run). Ensure the scaffolded
  models.yaml carries a working `default:` skeleton so a keyed provider passes doctor.

## Cluster 2 — CONFIG AS CLOSED STRUCTURE + DISCOVERY (D2 + D3 + D4). Files: `gateway_config.rs`, `main.rs`, `gateway.rs`, `preflight.rs`.
- **D2 — misplaced/unknown key detection** (config is `Value`-parsed, no `deny_unknown_fields`): in
  `check`, scan for `models_yaml` anywhere other than `gateway.models_yaml` (e.g. top-level or under
  `praxec:`) → error "`models_yaml` under `<block>:` is ignored — did you mean `gateway.models_yaml`?".
  Reject unknown keys in the known blocks (`gateway:`, `praxec:`) against their allowed set. Additive.
- **D3 — `praxec schema models-config`:** the models.yaml config has a typed struct (affinity_resolver /
  model_resolver). Add its schemars-derived JSON Schema as a new arm of `praxec schema` (mirror the
  existing `audit-event` arm). One subcommand.
- **D4 — echo the in-force config path everywhere:** `serve` startup banner, `health`, and `doctor`
  print the ABSOLUTE resolved config path. `doctor` additionally notes when a DIFFERENT config is
  discoverable at a conventional location and differs (the two-config trap).

## Cluster 3 — LIFECYCLE + WARNING HYGIENE (D5 + D6). Files: repo.rs, validate.rs, the describe/query response, definition model.
- **D5 — surface `lifecycle`:** include the definition's `lifecycle` in the `describe`/`query` response
  and in the `start`/`command` response. Add a `lifecycle` value that reads as placeholder (e.g. `stub`
  / `spec-only`) which `check` reports as a SOFT WARNING and `start` echoes — a placeholder executor
  must not be structurally identical to a working one. Keep the existing H11b (validate.rs:3657) lifecycle
  presence check. (Also: the design pack's flow.anneal now uses the real cap — the stub confusion was a
  currency bug, already fixed by publishing; this is the general structural fix.)
- **D6 — `reference_only` manifest opt-in:** `repo.rs:364` warns `UNSCANNED_DEFINITION_DIR` when a tier
  maps to a dir holding YAML that isn't loaded. Let a manifest declare the mapping INTENTIONAL (e.g.
  `layout: { connections: { dir: ._unused, reference_only: true } }` or a `reference_only: [connections]`
  list) → suppress the warning FOR THAT TIER ONLY. Update the two official packs' manifests to use it.
  Never blanket-suppress. Add a test: reference_only ⇒ no warn; a genuine unscanned dir ⇒ still warns.

## Verification (per cluster + final)
- `cargo test -p <crate>` green (ONE cargo at a time). Red-first tests for each behavior
  (assert-don't-derive): the exact repro from the report → now errors/surfaces; the legitimate case →
  still passes (no false positive). Final: `cargo test` workspace green; clippy clean; and re-run each
  report repro (dangling path, wrong key, stub lifecycle, reference_only, schema, doctor-ends-init).
