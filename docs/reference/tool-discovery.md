# MCP tool discovery (Phase 1)

Read-only surface for discovering candidate MCP tools from configured
registries. Phase 1 provisions nothing — no installs, no secrets elicitation,
no config mutation. It exists so a caller (human or agent) can ask "what MCP
tools exist that could do X?" and get back a ranked, typed answer, without the
gateway hardcoding a registry of URLs in Rust.

See `docs/design/plans/2026-07-30-mcp-tool-discovery-phase1.md` for the full
implementation plan (Phase 1, Tasks 1–7). This page documents the shipped
config shape, the `ToolCandidate` schema, and the two `praxec.query` verbs.

## The two-tool surface (SPEC §32)

`discover` and `evaluate` are **not** new MCP tools — the gateway's public
surface stays exactly `praxec.query` / `praxec.command`. They are two more
field-shapes `praxec.query` dispatches by:

```jsonc
praxec.query { "discover": "<free-text query>" }
praxec.query { "evaluate": { "verbs": ["diagnose", "..."] } }
```

Both are exclusive shapes: mixing `discover` or `evaluate` with any other
`praxec.query` intent field (`query`, `subject`, `workflowId`, `transition`,
`definitionId`, `observe`, `approvals`, or each other) returns the structured
`AMBIGUOUS_INTENT` response with the current legal links — the same recovery
path every other malformed `praxec.query` call gets.

The `home` response (`praxec.query {}`) advertises a `discover` and an
`evaluate` HATEOAS link, so a caller who starts from `{}` can find the verb
without reading this doc.

## Configuring registries

Declare zero or more registries under a top-level `registries:` array. Each
entry needs a `kind` and a `name`; the rest of the shape depends on `kind`.

```yaml
registries:
  - kind: github-org
    name: praxec-org
    org: praxec

  - kind: static
    name: local
    candidates:
      - name: browser-mcp
        description: Playwright-backed browser automation MCP server.
        transport: stdio
        source:
          npm:
            pkg: "@playwright/mcp"
        verbs: [diagnose]
        tags: [browser, e2e]
        trust_tier: verified
```

### `kind: github-org`

Enumerates an organization's public GitHub repos via the GitHub REST API
(`GET /orgs/<org>/repos`) and maps each repo to one `ToolCandidate`:

- `name` — the repo name.
- `description` — the repo's GitHub description (empty string if unset).
- `transport` — `stdio` (the Phase-1 default; the adapter doesn't attempt to
  infer a different transport from the repo).
- `source` — `{ repo: { url: "https://github.com/<org>/<repo>" } }`.
- `trust_tier` — `org` (a step above `community`, below an operator-curated
  `verified` entry).
- `verbs` / `tags` — the repo's GitHub topics, split: a topic that is a known
  `cap_verb` (the same vocabulary workflow transitions use) becomes a `verbs`
  entry; every other topic becomes a `tags` entry.

| field | required | notes |
|---|---|---|
| `name` | yes | this registry's display name (provenance) |
| `org`  | yes | the GitHub org/user to enumerate |

**Best-effort.** A `github-org` registry that fails (network down, org not
found, rate-limited) does not fail the query — `assemble` downgrades it to a
warning string in the response's `warnings` array, and every other configured
registry still contributes its candidates.

### `kind: static`

Inline candidates, verbatim — no network, no adapter logic beyond stamping
`provenance` with this registry's `name`. Use this for tools you already know
about and want discoverable without waiting on a live registry (or for a
config that must work fully offline, like this doc's example).

| field | required | notes |
|---|---|---|
| `name` | yes | this registry's display name (provenance) |
| `candidates` | yes | array of inline `ToolCandidate` objects (see schema below) |

### Unknown `kind`

An entry whose `kind` isn't recognized becomes a warning at assembly time
("registry '<name>' has unknown kind '<kind>' — skipped"), never a hard
config error — new registry kinds (Phase 1.5/3: `mcp-registry`, Smithery,
Glama, PulseMCP, a generic `rest` adapter) are additive.

## `ToolCandidate` schema

Every adapter normalizes to this one shape:

```jsonc
{
  "name": "browser-mcp",
  "description": "Playwright browser automation",
  "transport": "stdio",              // stdio | docker | remote | rest
  "source": { "npm": { "pkg": "@playwright/mcp" } },
  // source is one of: repo{url} | crate{name} | npm{pkg} | image{image} | url{url}
  "verbs": ["diagnose"],             // cap_verb vocabulary — what it can serve
  "tags": ["browser", "e2e"],        // everything else, free-form
  "trust_tier": "verified",          // community | org | verified
  "requires": { "secrets": [], "config": [] },  // Phase 2 provisioning hint, advisory only
  "provenance": "local"              // which registry surfaced it
}
```

`trust_tier` also drives dedup: when the same tool (same `name` + same
`source`) is surfaced by more than one registry, `assemble` keeps the
highest-trust copy.

## The two verbs

### `discover`

```jsonc
praxec.query { "discover": "browser" }
```

Assembles the catalog from every configured registry, then ranks it against
the free-text query: a substring hit on `name` scores highest, then
`description`, then `tags`/`verbs`. An empty string (`{ "discover": "" }`) is
a legal input — it returns the whole assembled catalog, unranked-filtered (no
zero-score drops). A query that matches nothing returns an empty `items`
array (not an error) plus the current legal links.

Response shape:

```jsonc
{
  "query": "browser",
  "items": [ /* ranked ToolCandidate objects */ ],
  "warnings": [ /* any registry that failed to assemble */ ],
  "links": [ { "rel": "home", "method": "praxec.query", "args": {} } ]
}
```

### `evaluate`

```jsonc
praxec.query { "evaluate": { "verbs": ["diagnose", "review"] } }
```

Assembles the catalog the same way, then ranks by cap-verb overlap:
candidates whose `verbs` intersect the requested list at all, sorted by
intersection size (descending) then `trust_tier` (descending). A candidate
with zero overlap is dropped. Deterministic — no relevance scoring beyond
those two keys.

Response shape mirrors `discover`'s: `{ "verbs": [...], "items": [...],
"warnings": [...], "links": [...] }`.

## Notes for operators

- **Not a hot path.** Phase 1 assembles the catalog fresh on every
  `discover`/`evaluate` call — there is no persistent cross-call cache yet.
  The `Cache` type (24h TTL) exists in `praxec_core::tool_catalog` for a
  later optimization pass but isn't wired into the query path.
- **No secrets, no installs.** `requires` on a candidate is advisory metadata
  for a future Phase 2 (`flow.tools.*` provisioning) — Phase 1 never reads or
  acts on it.
- **All IO is best-effort.** A `github-org` registry (or, later, any
  network-backed registry kind) degrades to a warning on any failure — a
  flaky network never turns `discover`/`evaluate` into a hard error.
