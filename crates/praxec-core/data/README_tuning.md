# tuning.json — recommendation & cost tuning

The knobs that shape model recommendation and cost estimation, as **configuration
rather than constants compiled into the source**. Tune the recommendation to your
judgement without rebuilding. Every field has a default, so an override file may
set only the knobs it cares about.

## Override precedence (shared `core::catalog`)

1. `$PRAXEC_TUNING_FILE`
2. `<config-dir>/praxec/tuning.json`
3. `./.praxec/tuning.json`
4. the shipped default (`data/tuning.json`)

## Knobs

| Field | Default | What it does |
|---|---|---|
| `affinity_weight` | `0.5` | In `affinity_fit`, how much the needed-affinity scores weigh vs overall intelligence. Raise toward `1.0` to favour task **specialists**; lower toward `0.0` to favour generally-capable models. |
| `sufficient_intelligence` | `52.0` | The reliability bar — models at/above it are "good enough"; most stances stay above it. |
| `default_requests_per_day` | `1000` | Volume assumed for the budget-ceiling check before the user dials their own. |
| `requests_per_day_levels` | `[100,1000,10000,100000]` | The presets the gate cycles through (←→). |
| `cost_input_tokens_per_request` | `5000` | Assumed prompt tokens per request (system + transcript + tool schemas). |
| `cost_output_tokens_per_request` | `600` | Assumed answer tokens per request (before reasoning). |
| `blended_price_input_weight` | `0.3` | Input's weight in the blended cost-ranking price (output weight = `1 - this`). |
| `reasoning_multipliers` | map | Output-token multiplier per reasoning level (more thinking → more billed tokens). An unlisted level falls back to `medium`. |
| `cost_magnitude_thresholds_usd_per_day` | `[0.10,1,10,100,1000,10000]` | The USD/day boundaries between cost buckets (pennies / tens-of-cents / dollars / … a day). Redefine what "pennies a day" means. |
| `reasoning.anthropic_budget_tokens` | map | Anthropic extended-thinking token budget per level (`0` = thinking off). |
| `reasoning.openai_effort` | map | OpenAI / OpenRouter `reasoning.effort` value per level. |
| `reasoning.gemini_level` | map | Gemini `thinking_level` value per level. |

The `reasoning.*` maps set the **values** sent in each provider's native
`additional_params` (the JSON *shape* is fixed by each provider's API).

## Example: favour cheap specialists at the executor

```json
{ "affinity_weight": 0.75 }
```

Everything else keeps its default. With a higher `affinity_weight`, a `needs: [coding]`
step is more likely to pick a coding specialist over a pricier frontier generalist.
