//! Build a `Vec<DiscoveryItem>` from a parsed gateway config.
//!
//! Honors the `discovery.include` config knob (`["proxy", "workflows",
//! "connections"]` by default for proxy + workflows, capped to those listed).

use anyhow::bail;
use serde_json::Value;

use crate::discovery::{DiscoveryItem, DiscoveryKind, DiscoveryLink};
use crate::proxy_workflow::DEFAULT_PROXY_WORKFLOW_ID;

/// The set of tokens accepted in `discovery.include`. A token outside this set
/// is almost always a typo (e.g. `workflow` for `workflows`) which would
/// silently drop a whole category from the index — so we reject it rather than
/// ignore it (CMP-031).
const KNOWN_INCLUDE_TOKENS: &[&str] = &["proxy", "workflows", "connections"];

pub fn index_from_config(config: &Value) -> anyhow::Result<Vec<DiscoveryItem>> {
    let include = config
        .pointer("/discovery/include")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| vec!["proxy".into(), "workflows".into()]);

    // CMP-031 — validate include tokens against the known set. An unrecognized
    // token yields a partial/empty index that looks like a silent config bug;
    // fail fast with a named error instead.
    for token in &include {
        if !KNOWN_INCLUDE_TOKENS.contains(&token.as_str()) {
            bail!(
                "INVALID_DISCOVERY_INCLUDE: unknown discovery.include token '{token}'; \
                 supported: {}",
                KNOWN_INCLUDE_TOKENS.join(" | ")
            );
        }
    }

    let mut items = Vec::new();

    if include.iter().any(|s| s == "workflows") {
        if let Some(map) = config.pointer("/workflows").and_then(Value::as_object) {
            for (id, def) in map {
                items.push(workflow_item(id, def));
            }
        }
    }

    if include.iter().any(|s| s == "proxy") {
        if let Some(arr) = config.pointer("/proxy/expose").and_then(Value::as_array) {
            for exposure in arr {
                if let Some(item) = capability_item(exposure) {
                    items.push(item);
                }
            }
        }
    }

    if include.iter().any(|s| s == "connections") {
        if let Some(map) = config.pointer("/connections").and_then(Value::as_object) {
            for (name, conn) in map {
                items.push(connection_item(name, conn));
            }
        }
    }

    // Skills are always indexed when present — they have no opt-out switch in
    // `discovery.include` because they exist only when the author declares a
    // `skills:` block, which is itself the opt-in (SPEC v2 §5.3).
    if let Some(skills) = config.pointer("/skills").and_then(Value::as_object) {
        for (subject, entry) in skills {
            items.push(guidance_item(subject, entry));
        }
    }

    // SPEC §22 — scripts are always indexed when present, same opt-in
    // reasoning as skills. The DiscoveryKind::Script variant keeps them
    // distinct from guidance in search results (and lets gateway.describe
    // route correctly based on kind).
    if let Some(scripts) = config.pointer("/scripts").and_then(Value::as_object) {
        for (subject, entry) in scripts {
            items.push(script_item(subject, entry));
        }
    }

    Ok(items)
}

/// SPEC §22 — convert a `scripts:` entry into a DiscoveryItem. Mirror of
/// [`guidance_item`] with two differences: kind is `Script` and the body
/// may come from either inline `body:` or external `uri:` (already
/// materialized into a `body` field by [`stamp_scripts_library`] at load
/// time — so by the time the indexer runs, every script has an inline
/// body in the snapshot).
///
/// Note: this indexer reads from the TOP-LEVEL `scripts:` block (which
/// still carries the original inline vs uri shape, NOT from the stamped
/// `_scriptsLibrary` on workflow snapshots). For uri-sourced scripts, the
/// inline body isn't present in the top-level entry — only in the
/// stamped library. We surface the `verb` + `source` regardless; `body`
/// is only populated when inline, and `gateway.describe(subject,
/// workflowId)` is the path to get a uri-sourced body (it reads from the
/// instance's stamped library, mirroring how guidance bodies are
/// resolved).
fn script_item(subject: &str, entry: &Value) -> DiscoveryItem {
    // CMP-overflow note: `verb` is `unwrap_or_default()` (empty string) rather
    // than erroring. This is safe because the indexer always runs on a config
    // that has already passed schema validation at load time (the `scripts:`
    // block requires `verb`); a missing `verb` here would be a loader bug, not
    // user input. An empty verb degrades the description string only, never the
    // index structure.
    let verb = entry
        .get("verb")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let body = entry
        .get("body")
        .and_then(Value::as_str)
        .map(str::to_string);
    let source = entry
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("config")
        .to_string();
    DiscoveryItem {
        id: subject.to_string(),
        kind: DiscoveryKind::Script,
        title: subject.to_string(),
        description: format!("Curated script '{subject}' (verb: {verb})."),
        tags: vec![],
        examples: vec![],
        aliases: vec![],
        text: format!("{subject} {verb}"),
        links: vec![],
        verb: Some(verb),
        body,
        source: Some(source),
    }
}

fn guidance_item(subject: &str, entry: &Value) -> DiscoveryItem {
    // CMP-overflow note: same as `script_item` — `verb` defaults to empty only
    // because this runs post config-load validation (the `skills:` block
    // requires `verb`). A missing value would indicate a loader bug, and the
    // empty fallback affects the description string only, not index structure.
    let verb = entry
        .get("verb")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let body = entry
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    // SPEC §5.3 — surface the fragment's provenance string. Default is
    // `config` (declared inline). Git-ingested fragments override this
    // when the config loader stamps a `source: "git+https://…"` value
    // onto the entry.
    let source = entry
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("config")
        .to_string();
    DiscoveryItem {
        id: subject.to_string(),
        kind: DiscoveryKind::Guidance,
        title: subject.to_string(),
        description: format!("Guidance fragment '{subject}' (verb: {verb})."),
        tags: vec![],
        examples: vec![],
        aliases: vec![],
        text: format!("{subject} {verb}"),
        links: vec![],
        verb: Some(verb),
        body: Some(body),
        source: Some(source),
    }
}

fn workflow_item(id: &str, def: &Value) -> DiscoveryItem {
    let title = def
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or(id)
        .to_string();
    let description = def
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut tags = string_array(def.get("tags"));
    let examples = string_array(def.get("examples"));
    let aliases = string_array(def.get("aliases"));

    // Thread the `process`/`taskClass` tag into the catalog so a flow is
    // filterable by task-class (via the existing tag search) — read back via
    // `DiscoveryItem::task_class`. Validity (non-empty, ≥1 outcome) is enforced
    // by `validate::v_process_metadata`; here we only index what's present.
    let process = def
        .get("process")
        .or_else(|| def.get("taskClass"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let mut text = Vec::<String>::new();
    if let Some(p) = process {
        tags.push(format!("{}{p}", crate::discovery::PROCESS_TAG_PREFIX));
        text.push(p.to_string());
    }
    if let Some(states) = def.pointer("/states").and_then(Value::as_object) {
        for (state_name, state) in states {
            text.push(state_name.clone());
            if let Some(d) = state.get("description").and_then(Value::as_str) {
                text.push(d.to_string());
            }
            if let Some(g) = state.get("goal").and_then(Value::as_str) {
                text.push(g.to_string());
            }
            if let Some(g) = state.get("guidance").and_then(Value::as_str) {
                text.push(g.to_string());
            }
            if let Some(ts) = state.pointer("/transitions").and_then(Value::as_object) {
                for (tname, t) in ts {
                    text.push(tname.clone());
                    if let Some(t_title) = t.get("title").and_then(Value::as_str) {
                        text.push(t_title.to_string());
                    }
                    if let Some(t_desc) = t.get("description").and_then(Value::as_str) {
                        text.push(t_desc.to_string());
                    }
                }
            }
        }
    }

    let input_schema = def.get("inputSchema").cloned();

    let mut start_args = serde_json::Map::new();
    start_args.insert("definitionId".into(), Value::String(id.to_string()));
    start_args.insert("input".into(), Value::Object(serde_json::Map::new()));

    DiscoveryItem {
        id: id.to_string(),
        kind: DiscoveryKind::Workflow,
        title,
        description,
        tags,
        examples,
        aliases,
        text: text.join(" "),
        links: vec![DiscoveryLink {
            rel: "start".into(),
            title: Some(format!("Start workflow '{id}'")),
            description: None,
            method: "praxec.command".into(),
            args: Value::Object(start_args),
            input_schema,
        }],
        verb: None,
        body: None,
        source: None,
    }
}

fn capability_item(exposure: &Value) -> Option<DiscoveryItem> {
    let name = exposure.get("name").and_then(Value::as_str)?.to_string();
    let title = exposure
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or(&name)
        .to_string();
    let description = exposure
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let tags = string_array(exposure.get("tags"));
    let examples = string_array(exposure.get("examples"));
    let aliases = string_array(exposure.get("aliases"));
    let input_schema = exposure.get("inputSchema").cloned();

    let mut start_args = serde_json::Map::new();
    start_args.insert(
        "definitionId".into(),
        Value::String(DEFAULT_PROXY_WORKFLOW_ID.to_string()),
    );
    start_args.insert("input".into(), Value::Object(serde_json::Map::new()));

    Some(DiscoveryItem {
        id: name.clone(),
        kind: DiscoveryKind::Capability,
        title,
        description,
        tags,
        examples,
        aliases,
        text: name.clone(),
        links: vec![DiscoveryLink {
            rel: "start_proxy_session".into(),
            title: Some("Start proxy_default to use this capability".into()),
            description: Some(format!(
                "After starting, submit transition '{name}' from the 'ready' state."
            )),
            method: "praxec.command".into(),
            args: Value::Object(start_args),
            input_schema,
        }],
        verb: None,
        body: None,
        source: None,
    })
}

fn connection_item(name: &str, conn: &Value) -> DiscoveryItem {
    let kind = conn
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_string();
    DiscoveryItem {
        id: format!("connection:{name}"),
        kind: DiscoveryKind::Connection,
        title: name.to_string(),
        description: format!("Configured {kind} connection '{name}'."),
        tags: vec![kind.clone()],
        examples: vec![],
        aliases: vec![],
        text: format!("{name} {kind}"),
        links: vec![],
        verb: None,
        body: None,
        source: None,
    }
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}
