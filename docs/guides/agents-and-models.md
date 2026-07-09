# Agents & models — the setup

praxec can put an LLM *inside* a workflow: a state declares `kind: agent` (or you
flip on `auto_drive`) and the runtime spawns a governed agent session to do the
work at that step. Two pieces of setup make that go:

1. **A models file** — `gateway.models_yaml` points at a `models.yaml` that says
   *which model* each agent binding resolves to.
2. **Provider keys** — the API key for whatever provider that model lives on.

Miss either and agent steps can't run. This guide wires both from scratch, then
covers the two ways to drive agents (`orchestrate` vs `auto_drive`), the
`auto_drive_tools` templating that hands a coding agent its repo, a few workflow
authoring gotchas, and a troubleshooting table keyed on the exact errors you'll
see.

If you just want the gateway to proxy tools with no in-runtime LLM, you don't
need any of this — skip it entirely.

---

## Quick start — a headless batch with a coding agent

Here's the whole thing end to end: a one-state workflow whose transition is a
`kind: agent` coding step, plus the config that lets it resolve a model and
authenticate. Everything below is copy-paste.

**1. Write a `models.yaml`** (canonical location `.praxec/models.yaml`):

```yaml
version: 1
default:
  - provider: { name: anthropic }
    model: claude-sonnet-4-6
overrides:
  coding-frontier:
    - provider: { name: anthropic }
      model: claude-opus-4-7
```

**2. Set a provider key** (writes `~/.praxec/providers.env`, mode 0600):

```bash
px set-provider-keys --anthropic sk-ant-...
# or, if you'd rather use the environment:
export ANTHROPIC_API_KEY=sk-ant-...
```

**3. Write the gateway config** (`gateway.yaml`) — note `gateway.models_yaml`
and the `kind: agent` transition:

```yaml
version: "1.0.0"

gateway:
  models_yaml: .praxec/models.yaml      # ← without this, agent steps can't resolve a model

store:  { kind: sqlite, path: ./praxec.sqlite }
audit:  { sink: file, path: ./audit.jsonl }

workflows:
  fix_readme:
    initialState: editing
    states:
      editing:
        transitions:
          fix:
            target: done
            actor: agent                 # ← an agent decides + runs this
            executor:
              kind: agent
              affinity: coding           # resolves via models.yaml (coding → coding-frontier → default)
              goal: "Tighten the README intro for {{ $.workflow.input.repo_path }}."
              tools: ["file:{{ $.workflow.input.repo_path }}"]
      done: { terminal: true }
```

**4. Validate, then run it:**

```bash
praxec check --config gateway.yaml      # fails fast if models_yaml is missing (AGENT_MODELS_YAML_REQUIRED)
praxec orchestrate --config gateway.yaml \
  --definition fix_readme \
  --input '{"repo_path": "/abs/path/to/repo"}' \
  --model anthropic:claude-sonnet-4-6 \
  --policy auto-approve
```

That's the shape. The rest of this guide is the detail behind each piece.

---

## The models file

Agent (and affinity-resolved `kind: llm`) steps declare a *binding* — an
affinity like `coding`, a tier like `frontier`, or a named activity — and the
runtime resolves it to a concrete `provider` + `model` through your models file.

**The runtime key is `gateway.models_yaml`.** That's the *only* key the runtime
reads for this — a top-level `models_yaml:` sitting at the root of your config is
inert and silently ignored. Point it at your file:

```yaml
gateway:
  models_yaml: .praxec/models.yaml
```

### Schema

`models.yaml` is the `ModelsFile` schema. `version` and a non-empty `default`
are mandatory; `overrides` and `strict_specificity` are optional:

```yaml
version: 1                               # only version 1 is supported
default:                                 # the fallback chain, tried in order
  - provider: { name: anthropic }
    model: claude-sonnet-4-6
overrides:                               # key = <affinity> | <tier> | <affinity>-<tier>
  coding-frontier:
    - provider: { name: openai }
      model: gpt-5
  coding:
    - provider: { name: anthropic }
      model: claude-sonnet-4-6
```

- **Affinities:** `coding`, `reasoning` (aliases: `math`, `science`), `prose`,
  `web-search`, `recon`.
- **Tiers:** `frontier`, `standard`, `commoditized`.
- **Override keys** are `<affinity>`, `<tier>`, or `<affinity>-<tier>`
  (e.g. `coding-frontier`). More specific wins.
- **Each binding is a list** — a chain. The resolver tries the first, and falls
  through to the next on failure. `default` is the last resort.
- Providers other than the known set use `provider: { name: custom, endpoint: "https://..." }`.

### Where it lives, and who finds it

- **The runtime** reads exactly the path in `gateway.models_yaml`.
- **`px doctor`** auto-discovers it: it looks for `.praxec/models.yaml` (project)
  first, then `~/.praxec/models.yaml` (user). A project file shadows the user one
  — `doctor` tells you when that's happening.
- **`flow.configure-models`** (the guided setup flow in `praxec-meta`) writes
  `.praxec/models.yaml` — same canonical name, same schema.

Keep the file at `.praxec/models.yaml` and everything lines up.

### Validate it

Round-trip any models file through the exact loader the runtime uses:

```bash
px validate-agents-config .praxec/models.yaml
# → {"ok": true, "summary": {...}}   (exit 0 on pass, 1 on fail)
```

### Without it

An agent step with no resolvable `gateway.models_yaml` can't pick a model.
`praxec check` now catches this **at load** rather than letting it blow up at the
first dispatch — see [Troubleshooting](#troubleshooting) for the exact errors.

---

## Provider keys

Once a binding resolves to `provider: anthropic, model: claude-sonnet-4-6`, the
runtime needs that provider's credential. Each provider reads its own env var:

| Provider     | Credential |
|--------------|-----------|
| Anthropic    | `ANTHROPIC_API_KEY` |
| OpenAI       | `OPENAI_API_KEY` |
| OpenRouter   | `OPENROUTER_API_KEY` |
| Gemini       | `GEMINI_API_KEY` |
| Ollama (local) | `OLLAMA_HOST` (keyless — points at your local server) |

### Two ways to supply them

**Environment** — set the var and go. Good for CI:

```bash
export ANTHROPIC_API_KEY=sk-ant-...
```

**File backend** — `~/.praxec/providers.env`, a flat `KEY=value` dotenv written
with mode `0600` inside a `0700` parent dir. Manage it with the CLI:

```bash
px set-provider-keys --anthropic sk-ant-... --openai sk-...
# no flags → interactively walks every supported provider
```

Override the path with `PRAXEC_PROVIDER_KEYS_FILE=/abs/path/keys.env` (useful when
`$HOME` isn't where you want secrets).

### Precedence and loading

- **The environment wins over the file.** If both set `ANTHROPIC_API_KEY`, the
  already-set env var stays — the file only fills in what's missing. CI can
  override a developer's file without touching it.
- **Both binaries load the file at startup.** `px` always did; the `praxec`
  gateway now loads `~/.praxec/providers.env` too (before the first `.await`, so
  no spawned task races the process env). So `praxec serve` and `praxec
  orchestrate` pick up your keys the same way `px walk` does.

A `kind: agent` step is an **in-process rig session** — no subprocess is spawned
— so it reads the *gateway process's* environment. Keys present when the gateway
started are the keys the agent sees.

---

## `orchestrate` vs `auto_drive` — two ways to drive agents

They sound similar and both put a model in the loop, but they operate at
different layers. You'll usually pick one.

### `praxec orchestrate --model <m>` — the mission chooser

`orchestrate` drives a *mission* toward its declared `outcomes`. At each step it
reads the current state and, when there's an agent-actionable decision to make,
asks a **transition chooser** (a model call) which legal transition to take, then
submits it — looping until the mission resolves, a human gate is declined, or it
hits the step bound.

```bash
praxec orchestrate --config gateway.yaml \
  --definition my_mission \
  --model anthropic:claude-sonnet-4-6 \
  --policy auto-approve
```

The `--model` is the *chooser's* model — a concrete `provider:model` string. This
is the top-level "which move next" brain, not the thing that edits your files.

### `praxec.agents.auto_drive: true` — the coding agent at the gate

`auto_drive` is a runtime setting. With it on, whenever a workflow reaches an
`actor: agent` gate, the runtime runs a coding agent to *perform* that step (it
resolves a model, hands the agent its tools, and projects the result), instead of
waiting for an external caller to submit the transition.

```yaml
praxec:
  agents:
    auto_drive: true
    auto_drive_affinity: reasoning        # binding used to resolve the agent's model (default: "reasoning")
    auto_drive_max_seconds: 300           # fail-fast bound per agent step (default: 0 = executor default)
    auto_drive_tools:                      # extra tools appended to every wired connection
      - "file:{{ $.workflow.input.repo_path }}"
```

- `auto_drive` (bool, default `false`) — the master switch.
- `auto_drive_affinity` (default `"reasoning"`) — the binding the agent's model
  resolves through in `models.yaml`.
- `auto_drive_tools` (list) — appended to the set of *every wired connection* the
  auto-driven agent gets. See templating below.
- `auto_drive_max_seconds` — a per-step deadline so a non-converging agent
  surfaces in minutes, not at the 600s executor default.

**Both need `gateway.models_yaml`.** Orchestrate's chooser takes its model from
`--model`, but any config that declares `kind: agent` steps or turns on
`auto_drive` still resolves agent model bindings through `models.yaml` — and
`praxec check` refuses to load such a config without it.

---

## `auto_drive_tools` templating

`auto_drive_tools` entries are `{{ … }}` templates, **rendered per leaf** against
that step's blackboard — `$.workflow.input.*` and `$.context.*`. This is how one
static config line hands each agent the *right* repo:

```yaml
auto_drive_tools:
  - "file:{{ $.workflow.input.repo_path }}"
```

At each agent step the runtime substitutes the value, so a run started with
`--input '{"repo_path": "/home/me/markdown-mcp"}'` gives that agent a
`file:/home/me/markdown-mcp` tool — scoped `read_file` / `write_file` rooted
there. A non-coding leaf with no `repo_path` in scope simply gets no file tool
(the composite tool host drops the unresolved one), so you can leave the entry in
for every step.

Two things to know:

- **The `agent.invoked` audit event logs the RAW, un-rendered template.** If you
  `audit tail` and see `file:{{ $.workflow.input.repo_path }}`, that's cosmetic —
  the event records the configured template; the *actual* rendering happens inside
  the agent executor a beat later. The tool the agent gets is the resolved path.
- **An unresolved key degrades to a `(key: unset)` stub** rather than erroring.
  `file:{{ $.workflow.input.repo_path }}` with no `repo_path` in scope renders to
  `file:(repo_path: unset)` — a nonsense tool that won't root anything. So **seed
  the input key**: pass it in `--input`, or map it into `$.context` upstream.

---

## Workflow authoring gotchas

A few things that error at load (so `praxec check` catches them) or behave in a
way that surprises people driving agents:

- **State `skills:` must be declared in the top-level `skills:` library.** A state
  that references a skill not present in the workflow's top-level `skills:` block
  (SPEC §11) is a load error. Declare the skill once in the library, reference it
  from states.

- **A workflow with `outcomes:` needs a success terminal.** If you declare
  `outcomes:` (the measurable definition of done) but no terminal state carries
  `outcome: success`, that's a load error — the outcomes have nowhere to resolve.

- **Branch templates only substitute whole-string markers.** In a dynamic
  `for_each` fan-out, `$.branch.value` and `$.branch.index` are replaced only when
  they're the *entire* value of a field — there's no embedded interpolation. Use
  `symbol: "$.branch.value"`, not `symbol: "prefix-$.branch.value"`.

- **A parallel fan-out is invisible to the `orchestrate` chooser.** A
  `for_each` / `kind: parallel` step runs all its branches *inside one
  transition* — the chooser sees a single move, not the N concurrent branches. If
  you want each branch's agent driven, put it behind a parent agent gate, or turn
  on `auto_drive` for the nested sub-workflows the branches run.

---

## Troubleshooting

| What you see | What it means | Fix |
|--------------|---------------|-----|
| `AGENT_MODELS_YAML_REQUIRED` at `praxec check` / serve | The config declares a `kind: agent` step or `auto_drive: true`, but `gateway.models_yaml` is unset or points at a missing file. | Set `gateway.models_yaml` to your `models.yaml` path. |
| `MODELS_YAML_LOAD_FAILED` at `praxec check` / serve | The file at `gateway.models_yaml` *exists* but won't parse/load. | Fix the YAML (or run `px validate-agents-config <path>` to see why), or remove the file. |
| `AGENT_NO_AGENTS_YAML` at first dispatch | An agent step tried to resolve a model with no usable `gateway.models_yaml`. (You'll normally hit `AGENT_MODELS_YAML_REQUIRED` at check first — this is the late signal if the check was skipped.) | Set `gateway.models_yaml`. |
| `orchestrate … the agentic driver's model call FAILED — <error>` | The chooser's model call itself failed — a real provider error (missing key, 401/auth, an unresolvable model binding, or a network fault), surfaced verbatim. This is **not** a dead-end flow. | Check your provider keys (`~/.praxec/providers.env` or the env var), `gateway.models_yaml`, and connectivity. |
| `orchestrate … found no actionable move and gave up` | The driver reached a state with no agent-actionable move — commonly a deterministic or human-gated flow, which `orchestrate` isn't meant to steer. | Drive it with `praxec command` / `query` (step-by-step) or `px walk` (end-to-end). If a fully-deterministic mission stalls, see [poka-yoke: orchestrate deterministic stall](../poka-yoke-orchestrate-deterministic-stall.md). |

---

## See also

- [Configuration reference](../reference/configuration.md) — `gateway.models_yaml`,
  the `auto_drive*` keys, and every other config knob.
- [The TUI agent](tui-agent.md) — driving workflows end-to-end with per-state
  sub-agents via `px walk`.
- [Checking workflows](checking-workflows.md) — `praxec check` / `fuzz` / `test`.
- [poka-yoke: orchestrate deterministic stall](../poka-yoke-orchestrate-deterministic-stall.md)
  — when a deterministic mission has no move for the agentic driver.
