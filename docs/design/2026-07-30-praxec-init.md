# `praxec init` — single-command onboarding

**Status:** draft for review · **Date:** 2026-07-30

## 1. Problem

A new user (the trigger: a Windows + Cursor user who "had a hard time") must, before praxec does anything, hand-produce **three** things:

1. a `gateway.yaml` (store, audit, repos, connections, the `models_yaml` pointer),
2. a `models.yaml` (provider:model bindings per affinity/tier),
3. the **editor MCP wiring** (Cursor's `.cursor/mcp.json` or Claude Code's `.mcp.json`, pointing at `praxec serve --config <path>`).

None of this is scaffolded. The only automation — `curl … packs/main/setup.sh | sh` — is **packs-only and not Windows-native** (`sh`). Every subcommand *requires* `--config` (no default path), so there isn't even a conventional home for the config. On Windows all three steps are worse (paths, no `sh`, manual Cursor JSON).

## 2. Goal

**`praxec init` → one command → a working, `doctor`-green setup + editor MCP wiring, cross-platform.** The new-user experience becomes: run `praxec init`, paste one API key, restart the editor, done.

The vehicle is a **subcommand of the binary** (not a shell script) on purpose: the Rust binary already runs natively on Windows/macOS/Linux and resolves per-OS paths via the `dirs` crate — a subcommand is the only "single-click" that doesn't fragment across `sh`/`.ps1`.

## 3. The command

```
praxec init [--editor cursor|claude|both|none] [--provider openrouter]
            [--dir <path>] [--force] [--yes]
```

- **Target dir** (default): `dirs::config_dir()/praxec/` — `~/.config/praxec` (Linux), `~/Library/Application Support/praxec` (macOS), `%APPDATA%\praxec` (Windows). `--dir` overrides. This also *establishes the conventional config home* praxec lacks today.
- **Safe + idempotent**: never overwrites an existing file without `--force`; prints exactly what it wrote vs skipped. Re-running is a no-op that re-checks + re-prints next steps.
- **`--yes`**: non-interactive (CI / scripted) — skip prompts, read the key from env.

## 4. What it scaffolds

### 4.1 `gateway.yaml` (sensible, durable defaults)
```yaml
version: "1.0.0"
gateway:
  principal: { subject: operator, roles: [human] }
  models_yaml: <dir>/models.yaml
praxec:
  embeddings: { enabled: false }         # the free lexical index; no flaky embed endpoint
  agents: { auto_drive: false }          # opt-in — a fresh user isn't auto-driving yet
audit:  { sink: file, path: <dir>/audit-logs, rotation: daily }
store:  { kind: sqlite, path: <dir>/praxec.db }   # durable by default (serve refuses ephemeral)
# repos: []        # add packs here — e.g. the praxec pack, or `praxec sync`
# connections: {}  # add MCP tools here — or provision via flow.tools.provision
```
Durable-by-default (sqlite + file audit) so `serve` starts without the ephemeral override; the two commented blocks are the only things a user grows into.

### 4.2 `models.yaml` (commodity defaults)
The battle-tested commodity chain (sourced from praxec's own defaults, sanitized — no operator-specific pools):
```yaml
version: 1
default:
  - provider: { name: openrouter }
    model: z-ai/glm-5.2
  - provider: { name: openrouter }
    model: deepseek/deepseek-v4-pro
    effort: high
  - provider: { name: openrouter }
    model: anthropic/claude-haiku-4-5
```
One provider (openrouter) → one key to set. The chain gives a commodity lead + a reasoning fallback out of the box.

### 4.3 `providers.env` (the one required secret)
Interactive prompt — "Paste your OpenRouter API key (or set `OPENROUTER_API_KEY`):" — written via the existing `provider_keys` writer to `<dir>/providers.env`. `--yes`/CI reads the env var and skips the prompt (noting it). This is the **only** thing a user must supply.

### 4.4 Editor MCP wiring (the Cursor fix)
For `--editor cursor` (and/or `claude`), write the MCP-server entry with **OS-correct absolute paths** to the resolved `praxec` binary + the scaffolded `gateway.yaml`:
```json
{ "mcpServers": { "praxec": {
    "command": "<abs path to praxec[.exe]>",
    "args": ["serve", "--config", "<abs path to gateway.yaml>"] } } }
```
- **cursor** → `.cursor/mcp.json` (project) by default, or the global Cursor MCP config with `--global` (Cursor supports both).
- **claude** → `.mcp.json`.
- Merge into an existing file (add/replace only the `praxec` key), never clobber other servers.
- This single step removes the manual Windows/Cursor wiring that was the reported pain.

### 4.5 `doctor` + next steps
Runs `praxec doctor --config <dir>/gateway.yaml`, prints the result, and a 3-line "you're ready: restart Cursor / try `praxec query {}`" epilogue. If the key was skipped (CI), it says exactly what to set.

## 5. Cross-platform (the Windows story)
- Config paths via `dirs` (already a dependency) → Windows `%APPDATA%`, macOS, Linux, all correct.
- Editor-config absolute paths resolved per-OS (incl. `praxec.exe`).
- The binary is the vehicle — no `sh`. Optional thin `setup.ps1`/`setup.sh` wrapper that only *downloads the release binary* then runs `praxec init` (a true double-click), but `init` itself is the substance.
- Release already ships all-OS binaries (confirmed), so the only prerequisite is "get the binary on PATH," which the wrapper handles.

## 6. Defaults sourced, not guessed
The `gateway.yaml` + `models.yaml` templates are the docs' quick-start shape + praxec's existing commodity model defaults, minus operator-specific repos/keys — i.e. *your* working config, sanitized, as the starter. Battle-tested, not invented.

## 7. Non-goals
- **Installing the binary** — the release + optional wrapper handle that.
- **Provisioning tools** — that's `flow.tools.provision` (the tool-lifecycle work); `init` gets you a working *gateway*, tools come after.
- **A GUI** — this is CLI-first; "single-click" = one command + one paste.

## 8. Open questions (for review)
- **Editor detection** — auto-detect an installed Cursor/Claude and default `--editor` to it, vs require the flag? (Lean: detect, confirm.)
- **Cursor project vs global** MCP config as the default (project is more discoverable; global is once-per-machine).
- **Starter packs** — ship `repos:` empty (clean) vs pre-wire a curated starter pack (e.g. the praxec meta pack) so the user has workflows on day one? (Lean: empty + a one-liner `praxec sync`/`repos:` hint, to keep it minimal.)
- **`auto_drive` default** — off (safe, explicit) vs on (works immediately but spends). Lean off.
