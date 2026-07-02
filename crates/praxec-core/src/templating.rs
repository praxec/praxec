use crate::model::WorkflowInstance;
use serde_json::Value;

/// Render a `goal` or `guidance` template string against a live workflow
/// instance.
///
/// Placeholder syntax: `{{` optional-whitespace `$.`-rooted-path
/// optional-whitespace `}}`.
///
/// Resolvable path prefixes:
/// - `$.context.*`                   → `instance.context`
/// - `$.workflow.input.*`             → `instance.input`
/// - `$.workflow.id`                  → `instance.id`
/// - `$.workflow.state`               → `instance.state`
/// - `$.workflow.version`             → `instance.definition_version`
/// - `$.praxec.authoring.*`         → operator's authoring preferences,
///   stamped onto the snapshot at config-resolve time (SPEC §17.x).
///   Advisory only — typical use:
///   `{{$.praxec.authoring.preferred_script_language}}` inside a skill
///   body so the LLM sees the operator's preferred language when
///   generating new scripts.
///
/// **Single-pass, non-recursive.** A substituted value that itself contains
/// `{{ … }}` is written verbatim into the output and is NOT re-scanned.
///
/// **Unresolved → stub.** `{{ $.context.missing }}` renders as
/// `(missing: unset)` — last path segment + `: unset`. The response is
/// always produced; this function never fails.
pub fn render_template(template: &str, instance: &WorkflowInstance) -> String {
    let mut output = String::with_capacity(template.len());
    let mut remaining = template;

    while let Some(start) = remaining.find("{{") {
        // Append everything before the opening `{{`.
        output.push_str(&remaining[..start]);
        let after_open = &remaining[start + 2..];

        let Some(end_rel) = after_open.find("}}") else {
            // No closing `}}` — emit the rest literally and stop.
            output.push_str(&remaining[start..]);
            return output;
        };

        let inner = after_open[..end_rel].trim();
        if inner.is_empty() {
            // Empty placeholder `{{}}` — not a valid token; emit verbatim.
            output.push_str("{{}}");
        } else {
            let replacement = resolve_template_path(inner, instance);
            output.push_str(&replacement);
        }

        // Advance past the closing `}}`.
        remaining = &after_open[end_rel + 2..];
    }

    // Append any tail after the last placeholder.
    output.push_str(remaining);
    output
}

/// Resolve a single trimmed path token (e.g. `$.context.someKey`) against
/// the instance. Returns the string representation of the matched JSON value,
/// or a `(lastSegment: unset)` stub when the path cannot be resolved.
pub(crate) fn resolve_template_path(path: &str, instance: &WorkflowInstance) -> String {
    // Scalar instance metadata — no further traversal needed.
    if path == "$.workflow.id" {
        return instance.id.clone();
    }
    if path == "$.workflow.state" {
        return instance.state.clone();
    }
    if path == "$.workflow.version" {
        return instance.definition_version.clone();
    }

    // SPEC §17.x — `$.praxec.authoring.*` resolves against the snapshot's
    // stamped `_authoringPrefs`. This is gateway-level operator preferences
    // (e.g. `preferred_script_language`), pinned at workflow.start time.
    let (root, tail) = if let Some(t) = path.strip_prefix("$.context.") {
        (&instance.context, t)
    } else if let Some(t) = path.strip_prefix("$.workflow.input.") {
        (&instance.input, t)
    } else if let Some(t) = path.strip_prefix("$.praxec.authoring.") {
        match instance.definition.pointer("/_authoringPrefs") {
            Some(prefs) => (prefs, t),
            None => {
                let last = path.rsplit('.').next().unwrap_or(path);
                return format!("({last}: unset)");
            }
        }
    } else {
        // Unrecognised prefix → stub using last segment of the path.
        let last = path.rsplit('.').next().unwrap_or(path);
        return format!("({last}: unset)");
    };

    let pointer = crate::guards::path_to_pointer(tail);
    match root.pointer(&pointer) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) => "(null)".to_string(),
        Some(v) => v.to_string(),
        None => {
            // Last dot-separated segment as the stub label.
            let last = tail.rsplit('.').next().unwrap_or(tail);
            format!("({last}: unset)")
        }
    }
}
