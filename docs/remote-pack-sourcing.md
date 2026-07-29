# Remote pack sourcing (`uri:` + `ref:`)

A `repos:` entry loads a workflow/capability pack (a directory with a
`praxec.repo.yaml` manifest) into a gateway config. There are two ways to
point at one:

```yaml
repos:
  - path: /home/me/repos/cognitive-architectures     # you manage the clone
  - uri: "git+https://github.com/acme/workflows"     # praxec manages the clone
    ref: main
```

**`uri:`/`ref:` is the recommended default for consuming a shared pack.**
`path:` is for a pack you're actively authoring (or forced onto — see
below); `uri:` is for a pack you consume as a dependency, the same way you'd
add a crate to `Cargo.toml` rather than vendor its source.

## `ref:` semantics

`ref:` is anything `git fetch <remote> <ref>` accepts:

| `ref:` value | Meaning | Moves when |
|---|---|---|
| a branch, e.g. `main` (the default) | tracks that branch's tip | every load, to whatever the branch currently points to |
| a tag, e.g. `v1.2.0` | a soft pin | only when you bump the tag in your config |
| a full commit SHA | an exact pin | never (that commit is immutable) |

All three work identically whether this is the first time praxec has seen
the `uri:` (a cold clone into a fresh cache) or a repeat load (a warm-cache
update). Prior to this increment, a cold clone only accepted a branch/tag —
a bare SHA on a never-before-seen cache failed with a git error
(`git clone --branch <sha>` isn't valid git). That's fixed: `clone_or_update`'s
cold-clone path now goes through `git init` + `git remote add` + `git fetch`
+ `git reset --hard FETCH_HEAD`, the same primitives the warm-cache update
path already used, so both paths accept a branch, a tag, or a bare SHA
uniformly. (A true reproducible, team-shared pin — independent of a branch
or tag ever moving upstream — is what the upcoming `praxec.lock` lockfile
increment is for; a hand-edited SHA in `ref:` works today but isn't tracked
or diffable the way a lockfile entry would be.)

## Auth: piggybacks entirely on your own git

Praxec never stores or manages git credentials. `clone_or_update`
(`crates/praxec-core/src/repo_git.rs`) shells out to your `git` exactly as
you'd invoke it yourself. If `git clone <uri>` (or `git fetch`) already
works in your shell — an SSH key loaded in your agent, an HTTPS credential
helper caching a token, `gh auth login` having configured a helper, a CI
runner's deploy key — it works identically for a `uri:` repo, headless or
interactive. **This means a private org pack repo just works with zero
praxec-side configuration.** There is nothing to set up beyond what your
normal git workflow already requires.

## Where the cache lives

A `uri:` repo is cloned/updated into:

```
<config-dir>/.praxec/repos/<slug>
```

where `<slug>` is a filesystem-safe, deterministic transform of the URI
(`repo_git::cache_dir_name`) — the same URI always maps to the same cache
directory, and it's re-fetched + hard-reset to the ref's tip on every config
load (`check`, `serve` start, and every hot reload). The cache is not meant
to be hand-edited; treat it like `target/` or `node_modules/` — if it ever
gets into a weird state, `rm -rf` it and reload.

Note: two `repos:` entries with the *same* `uri:` but *different* `ref:`
values (across configs, or even within one host's multiple gateways) share
one cache directory today and will fight each other on `reset --hard`. Give
each distinct `(uri, ref)` pair its own config if you need both checked out
simultaneously — this is a known sharp edge, not something this increment
changes.

## `path:` vs `uri:` — when to use which

| | `path:` | `uri:` |
|---|---|---|
| Who manages the clone | you (manual `git clone`/`git pull`) | praxec (`clone_or_update`) |
| Edits hot-reload | yes, ~10s TTL, zero ceremony | no — a remote repo isn't in the local-mtime tracked set; content only updates on the next `clone_or_update` (which itself only runs on a config load/reload) |
| Best for | actively authoring/editing the pack locally | consuming a pack someone else maintains |
| Pin visibility | invisible to the gateway config (whatever's on disk) | visible and diffable — the `uri:`/`ref:` pair lives in version control alongside the rest of your config |

If you're editing a pack's YAML files and want instant hot-reload, use
`path:`. If you're pulling in a pack as a dependency and want the pin
visible in your config (and don't want to remember to `git pull`), use
`uri:`.

## Example

See [`examples/remote-packs/gateway.yaml`](../examples/remote-packs/gateway.yaml)
for a runnable-shape config with both a moving branch ref and a tag pin.
That file isn't swept by CI's example-validation test on purpose — it points
at real network remotes, and that test suite must stay offline.

To see the whole path work end-to-end with **no network**, point `uri:` at
a local repo over `file://` — this is exactly how `repo_git.rs`'s own tests
prove the cold-clone and update paths without touching the network:

```bash
# Seed a local "remote".
mkdir -p /tmp/demo-origin && cd /tmp/demo-origin
git init -q -b main .
printf 'schema: praxec.repo/v1\nnamespace: demo\n' > praxec.repo.yaml
git add . && git -c user.email=t@t -c user.name=t commit -qm seed

# Point a gateway config at it over file://.
cat > /tmp/demo-gateway.yaml <<YAML
version: "1.0.0"
gateway:
  allow_ephemeral: true
  principal: { subject: operator, roles: [human] }
praxec:
  _writableRepos: [{ root: ".", push: false }]
repos:
  - uri: "file:///tmp/demo-origin"
    ref: main
YAML

praxec check --config /tmp/demo-gateway.yaml
```

`praxec check` cold-clones `/tmp/demo-origin` into
`/tmp/.praxec/repos/<slug>` and validates the loaded (empty, in this
minimal demo) pack — proving the `uri:`/`ref:` path resolves without any
network access. Swap `ref: main` for the commit SHA printed by
`git -C /tmp/demo-origin rev-parse HEAD` to see the bare-SHA cold-clone
path work identically.
