# Publishing & registry listings

How praxec gets published and where it gets listed. This file is
the source of truth for the metadata used across every directory — keep
the blurbs below in sync with `server.json` and the README.

## Status

praxec is **not yet published to any package registry**. No `v*`
tag has been cut, so there is no crates.io crate, no GHCR image, and no
GitHub Release. Every listing step below depends on a release existing
first — see step 1.

## Canonical metadata

Reuse this verbatim everywhere so listings stay consistent:

| Field       | Value |
|-------------|-------|
| Name        | `praxec` |
| Registry name | `io.github.praxec/praxec` |
| Tagline     | MCP gateway that fronts any number of tools, CLIs and APIs behind two governed, audited tools. |
| Repository  | https://github.com/praxec/praxec |
| Language    | Rust |
| License     | Apache-2.0 |
| Categories  | Aggregator / Gateway / Proxy |
| Tags        | `mcp`, `gateway`, `proxy`, `hateoas`, `workflow`, `governance`, `audit`, `rust` |

## 1. Cut a release (prerequisite for everything else)

```bash
git tag v0.0.15
git push origin v0.0.15
```

The `v*` tag triggers three workflows:

| Workflow          | Produces |
|-------------------|----------|
| `release.yml`     | Cross-platform binaries + checksums on the GitHub Release |
| `publish.yml`     | All workspace crates on crates.io |
| `publish-mcp.yml` | GHCR image **and** the official MCP Registry entry (new) |

Confirm the GHCR image is public afterward: GitHub → repo → Packages →
`praxec` → Package settings → set visibility to **Public**. The
registry cannot verify a private image.

## 2. Official MCP Registry

This is the high-leverage step — several directories below feed off it.

It is **automated** by `.github/workflows/publish-mcp.yml`: the workflow
builds the image, then runs the official `mcp-publisher` CLI with GitHub
OIDC (no stored secrets). The manifest is `server.json` in the repo root.

To publish **manually** instead (e.g. a first run, before trusting CI):

```bash
# install the official CLI
brew install mcp-publisher        # or download from the registry releases

mcp-publisher login github        # device-code flow, your GitHub account
mcp-publisher publish             # reads ./server.json
```

Verify it landed:

```bash
curl "https://registry.modelcontextprotocol.io/v0.1/servers?search=io.github.praxec/praxec"
```

Notes:
- The registry only stores **metadata** — it points at the GHCR image,
  it does not host it.
- Namespace `io.github.praxec/*` is granted by GitHub auth because
  you own the repo. The `name` in `server.json` must keep that prefix.
- Ownership of the image is proved by the
  `LABEL io.modelcontextprotocol.server.name` line in the `Dockerfile`.

## 3. Directories that need NO action

**PulseMCP** and **Glama** index automatically from the official
registry and from GitHub. Once step 2 is done they pick up praxec
within a few days. No submission required — don't waste time on forms
for these.

## 4. Directories that need manual submission

All of these are outward-facing actions under your GitHub identity, so
they are left for you to do. The text to paste is ready below.

### awesome-mcp-servers (pull request)

Repo: https://github.com/punkpeye/awesome-mcp-servers — add the entry to
the **Aggregators** section (servers that expose multiple tools through
one MCP server), keeping alphabetical order.

```markdown
- [praxec/praxec](https://github.com/praxec/praxec) 🦀 🏠 - MCP gateway that fronts any number of tools, CLIs and APIs behind two governed, audited tools.
```

(`🦀` = Rust, `🏠` = runs locally — matches that list's emoji legend.)

PR description:

```
Add praxec to Aggregators

praxec is an MCP gateway: it fronts any number of MCP servers,
CLI commands, and REST APIs while exposing a fixed surface of two
tools to the model. Capabilities are reached through search and
HATEOAS-style response links instead of a flat tool list, and every
call is schema-validated, guard-checked, and audited. Apache-2.0, Rust.
```

### mcp.so

Submit at https://mcp.so/submit (GitHub-based). Fields:

- **URL:** `https://github.com/praxec/praxec`
- **Description:** MCP gateway that fronts any number of tools, CLIs and APIs behind two governed, audited tools. The model sees a fixed two-tool surface; capabilities are reached by search and HATEOAS links, every call schema-validated and audited.

### MCP.Directory

Submit at https://mcp.directory/submit — it auto-pulls metadata from the
GitHub repo and publishes within ~24h. Just give it the repo URL:
`https://github.com/praxec/praxec`.

### MCPCentral

Submit at https://mcpcentral.io/submit-server. MCPCentral also accepts
servers via the `mcp-publisher` CLI; if it has ingested from the
official registry by the time you check, no separate action is needed.

## 5. Smithery — partial fit

`smithery.yaml` is in the repo root and declares the **stdio** launch
form, which is enough for Smithery to list praxec and install it
locally via the Smithery CLI.

Smithery's **hosted** container deployment is a different thing: it
requires the server to speak MCP Streamable HTTP on a `/mcp` endpoint
and listen on `$PORT`. praxec serves over stdio only today, so
hosted deployment would need an HTTP server transport added to the
crate first. Until then, treat Smithery as a listing, not a host.

## A note on "submit everywhere" automation

There is no established, trustworthy CLI that auto-submits a server to
the official registry *and* a spread of third-party directories. The
only official tool is `mcp-publisher`, and it targets the official
registry only (used by `publish-mcp.yml` above). Treat any "one command,
listed everywhere" tool with suspicion — running an unvetted tool that
posts to many services under your identity is a real supply-chain risk.
The manual submissions in step 4 take a few minutes each and are worth
doing by hand.

## Checklist

- [ ] Bump the workspace version in `Cargo.toml` when cutting a release
- [ ] `git tag vX.Y.Z && git push origin vX.Y.Z`
- [ ] Confirm `release.yml`, `publish.yml`, `publish-mcp.yml` all pass
- [ ] Set the GHCR package visibility to Public
- [ ] Verify the registry entry via the API curl above
- [ ] Open the awesome-mcp-servers PR
- [ ] Submit to mcp.so, MCP.Directory, MCPCentral
- [ ] (Optional) Submit to Smithery as a stdio listing
- [ ] Spot-check PulseMCP and Glama a few days later — should appear on their own
