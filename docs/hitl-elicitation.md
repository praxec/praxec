# HITL elicitation: the human-gate contract

A transition-level `actor: human` transition parks the mission until a human
resolves it. This document is the contract for what that gate carries — the
question, the evidence, and the option set — and the fences that keep a human
from ever being asked to decide with missing or partial context.

Two resolution paths exist for every gate, always:

- **Push** — the MCP server sends an `elicitation/create` form to the client;
  the accepted answer becomes the governed submit's arguments.
- **Pull** — the operator resolves the gate by hand (CLI/approvals) with full
  JSON arguments via the `pending_human` handle.

The push path is fenced (see [Skip semantics](#skip-semantics)); the pull path
is always available.

## The prompt-source chain

A parked gate resolves its prompt through a fixed chain — first link wins:

1. the transition's `prompt` / `goal` / `title`;
2. the instance context's non-empty `prompt` string (the caller-seeded
   convention — a required-or-defaulted string `prompt` input lands here via
   input→context seeding);
3. the enclosing state's `goal`, rendered through the same template renderer
   as state guidance.

**V33 `HUMAN_GATE_NO_PROMPT_SOURCE`** proves at load time that at least one
link is statically guaranteed: a non-empty transition prompt key, a state
`goal`, or a `prompt` input that is required or defaulted, string-typed, with
a string default. A gate that passes `praxec check` can therefore never park
promptless — the old runtime fallback is unreachable on fresh config and is
instrumented as a validator↔runtime parity-breach signal, not deleted. The
`drop_prompt_source` mutation operator keeps this measured: it deletes every
link at once and asserts V33 kills the mutant.

## `presents:` — declared decision context

The gate transition declares which context values the human sees alongside
the question:

```yaml
transitions:
  pick:
    actor: human
    presents: ["$.context.candidates", "$.context.constraints"]
```

Each entry is exactly `$.context.<key>` — one key segment, no nesting, no
indexing. At park time the declared keys are projected from live context onto
the gate as `pending_human.presented`.

Projection is **all-or-nothing**. A malformed pointer, a key that resolves to
nothing, or a projection over the total byte budget marks the gate defective
(`PRESENTS_UNRESOLVED: …`) instead of showing a partial view — the mission
still parks, but no form is pushed (see Skip semantics). There is no partial
success: the human either sees everything that was declared, or a defect.

Rendering has its own per-value budget: inside the pushed elicitation message
an over-budget value is truncated with a self-announcing marker naming where
the full value lives (`pending_human.presented["<pointer>"]`), so a clipped
*display* is always visibly clipped and the full value stays reachable on the
gate.

**V34 `INVALID_PRESENTS`** rejects at load: a non-array declaration, a
non-string or non-`$.context.<key>` entry, a key nothing in the workflow can
have written (not an input, `initialContext` key, transition/`onEnter` output,
or `use.outputs` binding), or a `presents:` on a non-human transition — a
dead declaration nothing would ever surface.

## `choices:` — a typed option set

The gate transition declares that the answer is a pick from a live context
array:

```yaml
choices:
  field: chosen_id            # the string submit-argument the answer fills
  from: "$.context.candidates" # the context array the options come from
  value: id                    # per-element dot-path → the option's value
  title: summary               # optional per-element dot-path → display title
```

At park time the declaration resolves against live context into
`pending_human.choices` — the option set the elicitation form renders as a
single-select enum (titled when every option has a title). The chosen value is
submitted as a plain string in `arguments[field]`.

Three fences keep the choice honest:

- **V35 `INVALID_CHOICES`** (load): the declaration must parse — through the
  *same* parser the runtime projection and the submit guard use, so the
  validator can never accept a shape the runtime rejects — `field` must name a
  `type: string` property of the transition's `inputSchema`, and `from` must
  be a reachable `$.context.<key>`. On a non-human transition the declaration
  is dead and rejected.
- **`CHOICES_UNRESOLVED`** (park): a malformed declaration or an option source
  that does not resolve to a non-empty array of well-shaped elements
  defect-marks the gate — same all-or-nothing rule as `presents`.
- **`CHOICE_MISMATCH`** (submit): a submission whose `arguments[field]` is not
  among the options resolved from the *current* context is rejected — on the
  push (elicitation resume) and pull (hand-typed) paths alike. What was
  offered and what is accepted can never disagree.

### `pick` — preserving object contracts downstream

The human answers with a string, but downstream consumers often expect the
whole chosen element (a `chosen: object` output contract). The `pick`
output-mapping operator bridges the two:

```yaml
output:
  chosen:
    pick:
      from: "$.context.candidates"   # the same array the choices came from
      by: id                          # element dot-path to match on
      eq: "$.arguments.chosen_id"    # the submitted choice
```

`pick` selects the first element of `from` whose `by` path equals the resolved
`eq`. On a governed gate submit its no-match `Null` is unreachable: the
`CHOICE_MISMATCH` guard has already proven the chosen key is in-set before the
mapping runs. V27 descends into `pick`'s `from`/`eq` operands, so a typo'd
scope fails at load rather than coalescing to `Null` at runtime.

## Skip semantics

Form construction makes an explicit push/skip decision per gate:

- a **defect-marked** gate (`PRESENTS_UNRESOLVED` / `CHOICES_UNRESOLVED`) is
  never pushed — the form would be built on missing context;
- a declared `inputSchema` that is **not elicitation-compatible** (elicitation
  forms collect primitives only) while the submit `require`s fields — with no
  choice set to answer through — is never pushed: the fallback free-text
  form's Accept could never satisfy the submit's validation. A doomed Accept
  is a lie to the operator.

A skipped gate is not a stuck gate: the mission stays parked with its pull
handle and the skip reason, and the operator resolves it with full JSON
arguments. **V36 `ELICITATION_INCOMPATIBLE_GATE`** warns about the doomed
shape at load time — including the partially-doomed case where `choices:` is
declared but the schema also requires a non-primitive beyond the choice field.
It is a Warning, not an Error, precisely because pull-only object gates are
legitimate.

## Error-code reference

| Code | Stage | Severity | Meaning |
|------|-------|----------|---------|
| `HUMAN_GATE_NO_PROMPT_SOURCE` (V33) | load | Error | Human gate with no statically-guaranteed prompt source |
| `INVALID_PRESENTS` (V34) | load | Error | Malformed `presents:`, unreachable key, or declaration on a non-human transition |
| `INVALID_CHOICES` (V35) | load | Error | Malformed `choices:`, non-string `field` property, unreachable `from`, or declaration on a non-human transition |
| `ELICITATION_INCOMPATIBLE_GATE` (V36) | load | Warning | Required non-primitive schema no elicitation form can satisfy ("Accept can never succeed") |
| `PRESENTS_UNRESOLVED` | park | gate defect | `presents` projection failed (malformed/unresolvable/over budget) — gate not pushed |
| `CHOICES_UNRESOLVED` | park | gate defect | `choices` resolution failed (malformed declaration or bad option source) — gate not pushed |
| `CHOICE_MISMATCH` | submit | rejection | Submitted choice not among the live options |

## Migration recipe for pack authors

The V36 warning names the gates to migrate. The worked example is
`cap.gate.human-pick-shape` — the pick-one-candidate gate that used to require
the operator to hand-type a whole object.

**Before** — the gate demands `chosen: object`, which no elicitation form can
collect (V36 warns; push path skips; pull-only in practice):

```yaml
awaiting_human:
  goal: Surface the candidate list + prompt; wait for the operator's pick.
  transitions:
    pick:
      target: done
      actor: human
      inputSchema:
        type: object
        required: [chosen]
        properties:
          chosen:    { type: object }
          rationale: { type: string, default: "" }
      executor: { kind: human }
      output:
        chosen:    "$.arguments.chosen"
        rationale: "$.arguments.rationale"
```

**After** — the human picks a string from a rendered option set; `pick`
rebuilds the object, so the `chosen: object` snippet output (and every
downstream consumer) is untouched:

```yaml
awaiting_human:
  goal: Surface the candidate list + prompt; wait for the operator's pick.
  transitions:
    pick:
      target: done
      actor: human
      presents: ["$.context.candidates"]
      choices:
        field: chosen_id
        from: "$.context.candidates"
        value: id       # each candidate element carries an `id`
        title: name     # ...and optionally a display `name`
      inputSchema:
        type: object
        required: [chosen_id]
        properties:
          chosen_id: { type: string }
          rationale: { type: string, default: "" }
      executor: { kind: human }
      output:
        chosen:
          pick: { from: "$.context.candidates", by: "id", eq: "$.arguments.chosen_id" }
        rationale: "$.arguments.rationale"
```

The recipe in general:

1. Give each element of the option array a stable string key (an `id`) if it
   doesn't have one.
2. Replace the required object property with a required string property
   (`chosen_id`), and declare `choices:` pointing `from` the array, `value` at
   the key, `title` at whatever reads well.
3. Rebuild the object output with a `pick` over the same array, keyed by the
   submitted string — the workflow's declared outputs do not change.
4. Declare `presents:` for whatever context the decision actually needs.
5. Re-run `praxec check`: the V36 warning disappears, and V33–V35 now prove
   the gate's whole surface at load time.
