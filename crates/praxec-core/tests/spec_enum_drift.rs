//! SPEC §23 drift detection — assert that the verb/root tables in docs/reference/spec.md
//! match the Rust closed enums exactly. Without this test, SPEC and Rust
//! drift silently whenever someone updates one and forgets the other.
//!
//! Strategy: read docs/reference/spec.md at test time, slice it by section header, extract
//! every backtick-quoted lowercase token from the slice, and compare the
//! resulting set against `Verb::ALL_TOKENS` / `ScriptVerb::ALL_TOKENS` /
//! `BLESSED_*_ROOTS`.
//!
//! When this test fails, the assertion error names the specific tokens that
//! diverged (present in one source, absent from the other) so the fix is
//! mechanical.

use std::collections::HashSet;
use std::path::PathBuf;

use praxec_core::discovery::{ScriptVerb, Verb, BLESSED_SCRIPT_ROOTS, BLESSED_SUBJECT_ROOTS};

fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p
}

fn read_spec() -> String {
    let path = workspace_root().join("docs/reference/spec.md");
    std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "docs/reference/spec.md must exist at {}: {e}",
            path.display()
        )
    })
}

fn read_schema() -> String {
    let path = workspace_root().join("schemas/gateway-config.schema.json");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("schema must exist at {}: {e}", path.display()))
}

/// Slice `src` by the heading that starts with `marker_prefix`. Returns
/// content from the matched heading up to (but not including) the next
/// heading at the same or shallower depth. Panics if `marker_prefix` is
/// not found.
fn slice_section<'a>(src: &'a str, marker_prefix: &str) -> &'a str {
    let start = src.find(marker_prefix).unwrap_or_else(|| {
        panic!("section header '{marker_prefix}' not found in docs/reference/spec.md")
    });
    let after_start = &src[start..];
    // The header line's `#`-prefix tells us the depth. Find next header
    // at <= same depth.
    let header_depth = after_start.chars().take_while(|c| *c == '#').count();
    let body_start = after_start
        .find('\n')
        .map(|i| start + i + 1)
        .unwrap_or(src.len());
    let body = &src[body_start..];
    let mut next_header_offset = body.len();
    for (i, line) in body.split_inclusive('\n').enumerate() {
        let _ = i;
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            let depth = trimmed.chars().take_while(|c| *c == '#').count();
            if depth <= header_depth {
                let line_start_in_body = line.as_ptr() as usize - body.as_ptr() as usize;
                next_header_offset = line_start_in_body;
                break;
            }
        }
    }
    &body[..next_header_offset]
}

/// Extract the FIRST backtick-quoted token from every markdown-table data
/// row (lines starting with `| \``). Used for verb tables where the row
/// label is the verb token and subsequent columns may reference *other*
/// tokens that we must NOT confuse with the row's own verb.
///
/// `.*` suffix on blessed-root patterns is stripped.
fn extract_table_first_column_tokens(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("| `") {
            continue;
        }
        // After `| ` find the first `... ` between two backticks.
        let after = &trimmed[2..];
        let Some(first_bt) = after.find('`') else {
            continue;
        };
        let after_bt = &after[first_bt + 1..];
        let Some(close_bt) = after_bt.find('`') else {
            continue;
        };
        let raw = &after_bt[..close_bt];
        // Skip header separator rows (`---`).
        if raw.starts_with('-') {
            continue;
        }
        let token = raw.strip_suffix(".*").unwrap_or(raw);
        if !token
            .chars()
            .next()
            .map(|c| c.is_ascii_lowercase())
            .unwrap_or(false)
        {
            continue;
        }
        if token.contains('.') {
            continue;
        }
        let s = token.to_string();
        if seen.insert(s.clone()) {
            out.push(s);
        }
    }
    out
}

/// Extract every backtick-quoted token matching `[a-z][a-z0-9-]*` (with an
/// optional trailing `.*` for blessed-root patterns, which is stripped).
/// Order-preserving, deduplicated. Used for prose-heavy sections like
/// §5.4.2 where roots are mentioned across both a table and a paragraph.
fn extract_backtick_tokens(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut chars = text.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if c != '`' {
            continue;
        }
        // Collect until the next backtick or non-token char.
        let mut token = String::new();
        while let Some(&(_, nc)) = chars.peek() {
            if nc == '`' {
                chars.next();
                break;
            }
            if nc.is_ascii_lowercase() || nc.is_ascii_digit() || nc == '-' || nc == '.' || nc == '*'
            {
                token.push(nc);
                chars.next();
            } else {
                // Backtick content has something we don't care about
                // (e.g. `INVALID_VERB`). Drop the token entirely.
                token.clear();
                while let Some(&(_, nc2)) = chars.peek() {
                    chars.next();
                    if nc2 == '`' {
                        break;
                    }
                }
                break;
            }
        }
        if token.is_empty() {
            continue;
        }
        // Strip blessed-root `.*` suffix.
        let stripped: &str = token.strip_suffix(".*").unwrap_or(&token);
        // Must start with a letter (filter out `5.4.1` style tokens).
        if !stripped
            .chars()
            .next()
            .map(|c| c.is_ascii_lowercase())
            .unwrap_or(false)
        {
            continue;
        }
        // Reject anything still containing `.` (compound like `plan.specify`).
        if stripped.contains('.') {
            continue;
        }
        let s = stripped.to_string();
        if seen.insert(s.clone()) {
            out.push(s);
        }
    }
    out
}

/// Extract bare comma-separated tokens from §22.4 code block. Lines look like
/// `verb-mirror:    build, test, deploy, ...` — labels colon-prefixed.
fn extract_code_block_roots(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut in_block = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            in_block = !in_block;
            continue;
        }
        if !in_block {
            continue;
        }
        // Drop everything before the colon (the label).
        let payload = line.split_once(':').map(|x| x.1).unwrap_or(line);
        for tok in payload.split(',') {
            let tok = tok.trim();
            if tok.is_empty() {
                continue;
            }
            // Must be a kebab-only token.
            if !tok
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            {
                continue;
            }
            if seen.insert(tok.to_string()) {
                out.push(tok.to_string());
            }
        }
    }
    out
}

fn assert_set_eq(label: &str, expected: &[&str], actual: &[String]) {
    let expected_set: HashSet<&str> = expected.iter().copied().collect();
    let actual_set: HashSet<&str> = actual.iter().map(String::as_str).collect();
    let missing: Vec<&str> = expected_set.difference(&actual_set).copied().collect();
    let extra: Vec<&str> = actual_set.difference(&expected_set).copied().collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "{label}: drift between Rust enum and SPEC/schema.\n  \
         missing from SPEC (present in Rust): {missing:?}\n  \
         extra in SPEC (absent from Rust):   {extra:?}\n\
         Reconcile by updating the source that's wrong."
    );
}

// ── §5.4.1 — cognitive verbs table ────────────────────────────────────────

#[test]
fn spec_5_4_1_table_matches_verb_all_tokens() {
    let spec = read_spec();
    let section = slice_section(&spec, "#### 5.4.1");
    // Use first-column extractor so the disambiguation prose in column 3
    // (which references OTHER verb tokens) doesn't pollute the result.
    let tokens = extract_table_first_column_tokens(section);
    assert_set_eq("§5.4.1 cognitive verbs", Verb::ALL_TOKENS, &tokens);
}

// ── §22.3 — script verbs table ────────────────────────────────────────────

#[test]
fn spec_22_3_table_matches_script_verb_all_tokens() {
    let spec = read_spec();
    let section = slice_section(&spec, "### 22.3");
    let tokens = extract_table_first_column_tokens(section);
    assert_set_eq("§22.3 script verbs", ScriptVerb::ALL_TOKENS, &tokens);
}

// ── §22.4 — blessed script roots ──────────────────────────────────────────

#[test]
fn spec_22_4_code_block_matches_blessed_script_roots() {
    let spec = read_spec();
    let section = slice_section(&spec, "### 22.4");
    let tokens = extract_code_block_roots(section);
    assert_set_eq("§22.4 blessed script roots", BLESSED_SCRIPT_ROOTS, &tokens);
}

// ── §5.4.2 — blessed subject roots (table + prose) ────────────────────────

#[test]
fn spec_5_4_2_matches_blessed_subject_roots_bidirectionally() {
    // §5.4.2 splits roots across a table (`review.*`, `authoring.*`, etc.)
    // and a prose paragraph for the verb-mirror additions. Backtick
    // extraction with `.*` stripping captures both forms.
    //
    // Bidirectional check: every Rust root MUST be mentioned in §5.4.2,
    // AND every backtick token in §5.4.2 (excluding the well-known
    // exception list) MUST be in the Rust const. Catches SPEC-side rot
    // — e.g. a leftover root from a removed verb that nobody noticed.
    let spec = read_spec();
    let section = slice_section(&spec, "#### 5.4.2");
    let mut tokens = extract_backtick_tokens(section);

    // SPEC §5.4.2 prose mentions a couple of non-root tokens in
    // backticks for context: the `praxec.strict_namespacing` flag
    // reference and concrete subject examples like `plan.specify`. Filter
    // out anything that isn't a single-segment lowercase token, since
    // roots are single-segment.
    tokens.retain(|t| !t.contains('.') && !t.contains('_'));
    // Also filter out backticked schema tokens that aren't roots (e.g.
    // `INVALID_SUBJECT_ROOT` — already filtered above because of `_`,
    // but defensively keep only true lowercase-kebab tokens).
    tokens.retain(|t| {
        t.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    });
    // Explicit allow-list for non-root tokens that legitimately appear
    // in §5.4.2 prose: `true` (boolean example for strict_namespacing),
    // `subject` (the noun being defined). Anything else extra-in-SPEC
    // is real drift — a leftover root from a removed verb or a typo.
    const KNOWN_NON_ROOT_MENTIONS: &[&str] = &["true", "false", "subject"];
    tokens.retain(|t| !KNOWN_NON_ROOT_MENTIONS.contains(&t.as_str()));

    assert_set_eq(
        "§5.4.2 blessed subject roots",
        BLESSED_SUBJECT_ROOTS,
        &tokens,
    );
}

// ── JSON schema script verb enum ──────────────────────────────────────────

#[test]
fn schema_script_verb_enum_matches_script_verb_all_tokens() {
    let schema = read_schema();
    // The script verb enum is the second `"enum"` after the `"scriptFragment"`
    // anchor. Slicing from `"scriptFragment"` to next top-level `"definitions"`
    // entry would be most robust, but for our purposes searching for the
    // anchor + verb prefix is enough.
    let anchor = "\"scriptFragment\"";
    let start = schema
        .find(anchor)
        .expect("scriptFragment anchor must exist in schema");
    let tail = &schema[start..];
    let verb_idx = tail
        .find("\"verb\":")
        .expect("scriptFragment must declare verb");
    let after_verb = &tail[verb_idx..];
    let enum_open = after_verb.find('[').expect("verb must have an enum array");
    let enum_close = after_verb.find(']').expect("verb enum must close");
    let enum_body = &after_verb[enum_open + 1..enum_close];
    // Strip quotes + commas, collect tokens.
    let tokens: Vec<String> = enum_body
        .split(',')
        .map(|s| s.trim().trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
        .collect();
    assert_set_eq(
        "JSON schema scriptFragment.verb.enum",
        ScriptVerb::ALL_TOKENS,
        &tokens,
    );
}

// ── JSON schema skill verb enum (post-F9 tightening) ──────────────────────

#[test]
fn schema_skill_verb_enum_matches_verb_all_tokens() {
    let schema = read_schema();
    let anchor = "\"skillFragment\"";
    let start = schema
        .find(anchor)
        .expect("skillFragment anchor must exist in schema");
    let after_anchor = &schema[start..];
    let verb_decl_idx = after_anchor
        .find("\"verb\":")
        .expect("skillFragment must declare verb");
    let after_verb = &after_anchor[verb_decl_idx..];
    // Bound search to the verb's JSON object: from the first `{` after
    // `"verb":` to the matching `}`. Within that window only, look for
    // `pattern` (legacy) vs `enum` (post-F9).
    let obj_open = after_verb
        .find('{')
        .expect("verb declaration must open a JSON object");
    let obj_body = &after_verb[obj_open..];
    // Find matching close brace at depth 0.
    let mut depth: i32 = 0;
    let mut close_offset: Option<usize> = None;
    for (i, c) in obj_body.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    close_offset = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = close_offset.expect("verb declaration object must close");
    let inside = &obj_body[..=close];

    let has_pattern = inside.contains("\"pattern\":");
    let has_enum = inside.contains("\"enum\":");

    if has_pattern && !has_enum {
        // Pre-F9: schema is still pattern-based. Skip with a clear note so
        // the gap is visible until F9 lands.
        eprintln!(
            "skipping skill verb schema check: skillFragment.verb is still \
             pattern-based, not enum. Tighten with F9."
        );
        return;
    }
    let enum_open = inside.find('[').expect("verb.enum array must open post-F9");
    let enum_close = inside[enum_open..]
        .find(']')
        .expect("verb.enum array must close")
        + enum_open;
    let enum_body = &inside[enum_open + 1..enum_close];
    let tokens: Vec<String> = enum_body
        .split(',')
        .map(|s| s.trim().trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
        .collect();
    assert_set_eq(
        "JSON schema skillFragment.verb.enum",
        Verb::ALL_TOKENS,
        &tokens,
    );
}
