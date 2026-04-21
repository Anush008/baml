//! Applier — produce modified source files from a candidate's
//! [`OptimizableFunction`].
//!
//! The approach is regex/text-based (not IR-driven). We locate `function
//! NAME` in each source file, walk forward to its `prompt` keyword, parse
//! the raw-string delimiters (`#"..."#`, `##"..."##`, etc.), and splice in
//! the candidate's new prompt text between them. In parallel we rewrite
//! class-level metadata: `/// ...` docstrings above `class <Name>` for
//! `class.description`, and per-field `@description(...)` / `@alias(...)`
//! attributes inside the class body.
//!
//! This mirrors the fallback path in
//! `engine/baml-runtime/src/optimize/applier.rs::replace_prompt_in_file`.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;

use crate::candidate::{Candidate, ClassDefinition, OptimizableFunction, SchemaFieldDefinition};

/// Applies candidate changes to produce modified source files.
pub struct Applier {
    #[allow(dead_code)]
    root_path: PathBuf,
}

impl Applier {
    pub fn new(root_path: PathBuf) -> Self {
        Self { root_path }
    }

    /// Produce a modified copy of `original_sources` with the candidate's
    /// prompt edits applied. Files that don't mention `function NAME` are
    /// copied unchanged.
    pub fn generate_modified_files(
        &self,
        candidate: &Candidate,
        original_sources: &HashMap<PathBuf, String>,
    ) -> Result<HashMap<PathBuf, String>> {
        let mut modified = original_sources.clone();
        for content in modified.values_mut() {
            if let Some(new_content) = apply_prompt_change(content, &candidate.function) {
                *content = new_content;
            }
        }
        Ok(modified)
    }
}

/// Apply all changes from a candidate's [`OptimizableFunction`] to a single
/// file's source text. Returns `Some(new_content)` when at least one change
/// landed; `None` when nothing matched or nothing changed.
fn apply_prompt_change(source: &str, func: &OptimizableFunction) -> Option<String> {
    let mut current = source.to_string();
    let mut changed = false;

    if let Some(rewrite) = replace_prompt_in_file(&current, &func.function_name, &func.prompt_text)
    {
        current = rewrite;
        changed = true;
    }

    for class in &func.classes {
        if let Some(rewrite) = apply_class_description(&current, class) {
            current = rewrite;
            changed = true;
        }
        if let Some(rewrite) = apply_field_attrs(&current, class) {
            current = rewrite;
            changed = true;
        }
    }

    if changed { Some(current) } else { None }
}

/// If `class.description` is set, add or replace a `///` doc-comment block
/// immediately above the `class <ClassName>` line in `source`.
fn apply_class_description(source: &str, class: &ClassDefinition) -> Option<String> {
    let description = class.description.as_ref()?;
    let class_start = find_class_start(source, &class.class_name)?;
    let line_start = line_start_of(source, class_start);
    let indent_end = source[line_start..class_start]
        .char_indices()
        .find(|(_, c)| !c.is_whitespace())
        .map(|(i, _)| line_start + i)
        .unwrap_or(class_start);
    let indent = &source[line_start..indent_end];

    let (doc_start, doc_end) = existing_doc_block(source, line_start);
    let existing = &source[doc_start..doc_end];
    let new_doc = format_doc_comment(description, indent);

    if existing == new_doc {
        return None;
    }

    let mut out = String::with_capacity(source.len() + new_doc.len());
    out.push_str(&source[..doc_start]);
    out.push_str(&new_doc);
    out.push_str(&source[doc_end..]);
    Some(out)
}

/// Rewrite `@description` / `@alias` attributes on fields inside
/// `class <ClassName> { ... }` to match the candidate's field metadata.
///
/// For each field whose `description` or `alias` is `Some(...)`, the
/// corresponding attribute on the source field is replaced (if present)
/// or appended to the end of the field's declaration line (if absent).
/// `None` on a candidate field means "leave the source unchanged" — we
/// don't strip user-authored annotations the LLM simply didn't mention.
///
/// Returns `Some(new_source)` only when at least one field line actually
/// changed, so callers can tell a no-op from a real edit.
fn apply_field_attrs(source: &str, class: &ClassDefinition) -> Option<String> {
    let any_attrs = class
        .fields
        .iter()
        .any(|f| f.description.is_some() || f.alias.is_some());
    if !any_attrs {
        return None;
    }

    let class_start = find_class_start(source, &class.class_name)?;
    let rel_brace = source[class_start..].find('{')?;
    let body_start = class_start + rel_brace + 1;
    let body_end = find_matching_close_brace(source, body_start)?;

    let body = &source[body_start..body_end];
    let fields_by_name: HashMap<&str, &SchemaFieldDefinition> = class
        .fields
        .iter()
        .map(|f| (f.field_name.as_str(), f))
        .collect();

    let mut new_body = String::with_capacity(body.len());
    let mut changed = false;

    // Walk the class body line by line. `split_inclusive('\n')` preserves
    // each line's terminator, so reassembly is exact even if the file has
    // no trailing newline.
    for segment in body.split_inclusive('\n') {
        let trim_start_offset = segment.len() - segment.trim_start().len();
        let rest = &segment[trim_start_offset..];

        // Blank, comment, attribute continuation, or docstring lines are
        // never a field declaration — pass them through untouched.
        let first_non_ws = rest.trim_start();
        if first_non_ws.is_empty()
            || first_non_ws.starts_with("//")
            || first_non_ws.starts_with('@')
            || first_non_ws.starts_with('{')
            || first_non_ws.starts_with('}')
        {
            new_body.push_str(segment);
            continue;
        }

        let name_end = rest
            .char_indices()
            .find(|(_, c)| !is_ident_char(*c))
            .map(|(i, _)| i)
            .unwrap_or(rest.len());
        let name = &rest[..name_end];

        if let Some(field) = fields_by_name.get(name) {
            let rewritten = rewrite_field_line(
                segment,
                field.description.as_deref(),
                field.alias.as_deref(),
            );
            if rewritten != segment {
                changed = true;
            }
            new_body.push_str(&rewritten);
        } else {
            new_body.push_str(segment);
        }
    }

    if !changed {
        return None;
    }

    let mut out = String::with_capacity(source.len());
    out.push_str(&source[..body_start]);
    out.push_str(&new_body);
    out.push_str(&source[body_end..]);
    Some(out)
}

/// Walk forward from `from` counting brace nesting and return the index
/// of the `}` that closes the block opened just before `from`.
fn find_matching_close_brace(source: &str, from: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut depth: i32 = 1;
    let mut i = from;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Rewrite a single field declaration line to carry the requested
/// `@description` / `@alias` attributes. `None` for an attribute leaves
/// any existing version of it alone. Preserves a trailing `// comment`
/// and the line's terminating newline.
fn rewrite_field_line(
    segment: &str,
    description: Option<&str>,
    alias: Option<&str>,
) -> String {
    let (code, tail) = if let Some(stripped) = segment.strip_suffix('\n') {
        (stripped, "\n")
    } else {
        (segment, "")
    };
    let (body, comment) = split_trailing_comment(code);
    let mut body = body.to_string();
    if let Some(desc) = description {
        body = set_attr(&body, "description", desc);
    }
    if let Some(a) = alias {
        body = set_attr(&body, "alias", a);
    }

    let mut out = String::with_capacity(body.len() + comment.len() + tail.len());
    out.push_str(&body);
    out.push_str(comment);
    out.push_str(tail);
    out
}

/// Split `code` at the start of a trailing `//` comment, skipping over
/// `//` that appears inside a double-quoted string. Returns
/// `(code_body, comment_with_leading_whitespace)`.
fn split_trailing_comment(code: &str) -> (&str, &str) {
    let bytes = code.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            i += 1;
            while i < bytes.len() && bytes[i] != b'"' {
                if bytes[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            }
            continue;
        }
        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'/') {
            let mut start = i;
            while start > 0 && matches!(bytes[start - 1], b' ' | b'\t') {
                start -= 1;
            }
            return (&code[..start], &code[start..]);
        }
        i += 1;
    }
    (code, "")
}

/// Set (or replace) the `@<name>("<value>")` attribute on a single line.
/// If the attribute already exists, its argument list is replaced with
/// the new value; otherwise the attribute is appended after any existing
/// attributes on the line.
fn set_attr(line: &str, name: &str, value: &str) -> String {
    let needle = format!("@{name}(");
    if let Some(start) = find_attr_occurrence(line, name) {
        let open = start + needle.len();
        let close = find_matching_close_paren(line, open).unwrap_or(line.len());
        let mut out = String::with_capacity(line.len() + value.len());
        out.push_str(&line[..open]);
        out.push('"');
        out.push_str(&escape_baml_string(value));
        out.push('"');
        out.push_str(&line[close..]);
        out
    } else {
        let trimmed_end = line.trim_end().len();
        let trailing_ws = &line[trimmed_end..];
        let mut out = String::with_capacity(line.len() + 20 + value.len());
        out.push_str(&line[..trimmed_end]);
        out.push(' ');
        out.push_str(&needle);
        out.push('"');
        out.push_str(&escape_baml_string(value));
        out.push('"');
        out.push(')');
        out.push_str(trailing_ws);
        out
    }
}

/// Find `@<name>(` in `line`, skipping `@@<name>(` block-level attributes
/// (which don't live on field lines anyway, but we defend against them).
fn find_attr_occurrence(line: &str, name: &str) -> Option<usize> {
    let needle = format!("@{name}(");
    let mut search_from = 0;
    while let Some(rel) = line[search_from..].find(&needle) {
        let abs = search_from + rel;
        // `@@description(...)` is a block-level attribute; skip over it.
        if abs > 0 && line.as_bytes()[abs - 1] == b'@' {
            search_from = abs + needle.len();
            continue;
        }
        return Some(abs);
    }
    None
}

/// Given a position just after an opening `(`, return the matching `)`
/// index. Treats `"..."` as opaque so parens inside string args don't
/// throw off the count.
fn find_matching_close_paren(line: &str, from: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut depth: i32 = 1;
    let mut i = from;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1;
                }
                continue;
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Escape the subset of characters that would otherwise terminate a BAML
/// attribute string literal.
fn escape_baml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Find `class <name>` (optional generic params) as a word-boundary match.
/// Returns the byte index of the `c` in `class`.
fn find_class_start(source: &str, name: &str) -> Option<usize> {
    let mut search_from = 0;
    loop {
        let rel = source[search_from..].find("class")?;
        let abs = search_from + rel;
        if abs > 0 && is_ident_continue(source.as_bytes()[abs - 1]) {
            search_from = abs + "class".len();
            continue;
        }
        let after = abs + "class".len();
        let ws_end = skip_whitespace(source, after);
        if ws_end == after {
            search_from = after;
            continue;
        }
        let name_end = ws_end + name.len();
        if source.get(ws_end..name_end) == Some(name)
            && source
                .as_bytes()
                .get(name_end)
                .is_none_or(|b| !is_ident_continue(*b))
        {
            return Some(abs);
        }
        search_from = after;
    }
}

/// Return the byte offset of the start of the line that contains `pos`.
fn line_start_of(source: &str, pos: usize) -> usize {
    source[..pos]
        .rfind('\n')
        .map(|nl| nl + 1)
        .unwrap_or(0)
}

/// Find the range [start, end) of contiguous `///` doc-comment lines
/// immediately preceding `line_start`. Returns `(line_start, line_start)`
/// when no doc block is present.
fn existing_doc_block(source: &str, line_start: usize) -> (usize, usize) {
    let mut end = line_start;
    let mut start = line_start;
    let mut cursor = line_start;
    while cursor > 0 {
        let prev_nl = source[..cursor - 1].rfind('\n');
        let prev_line_start = prev_nl.map(|n| n + 1).unwrap_or(0);
        let prev_line = &source[prev_line_start..cursor - 1];
        if prev_line.trim_start().starts_with("///") {
            start = prev_line_start;
            cursor = prev_line_start;
        } else {
            break;
        }
    }
    // If we walked back at all, `end` should include the terminating newline
    // of the last doc line — which is the byte just before `line_start`.
    if start < line_start {
        end = line_start;
    }
    (start, end)
}

/// Render `description` as one or more `/// ...` lines, each indented to
/// match `indent`. Guarantees a trailing newline so the `class` line
/// follows on its own line.
fn format_doc_comment(description: &str, indent: &str) -> String {
    let mut out = String::with_capacity(description.len() + 16);
    for line in description.split('\n') {
        out.push_str(indent);
        out.push_str("/// ");
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Locate `function <func_name> { ... prompt #"..."# ... }` in `source` and
/// rewrite the prompt's contents to `new_prompt`, preserving the original
/// `#` delimiter count.
///
/// Returns `None` if the function isn't found, if the prompt's raw-string
/// delimiters can't be parsed, or if no change would be made.
pub fn replace_prompt_in_file(
    source: &str,
    func_name: &str,
    new_prompt: &str,
) -> Option<String> {
    let fn_start = find_function_start(source, func_name)?;
    let prompt_kw = find_keyword_after(source, fn_start, "prompt")?;
    let (open_end, hash_count) = parse_raw_string_open(source, prompt_kw)?;
    let close_start = find_raw_string_close(source, open_end, hash_count)?;

    let original = &source[open_end..close_start];
    let reindented = reindent_prompt(original, new_prompt);

    if reindented == original {
        return None;
    }

    let mut out = String::with_capacity(source.len() + reindented.len());
    out.push_str(&source[..open_end]);
    out.push_str(&reindented);
    out.push_str(&source[close_start..]);
    Some(out)
}

/// Match the indentation/newline shape of `original` when substituting in
/// `new_prompt`.
///
/// - Preserves the original's leading and trailing newlines (so the closing
///   `"#` stays on its own line if it was).
/// - Strips any common leading whitespace from `new_prompt` (in case the
///   reflection model already indented it).
/// - Re-indents every non-empty line of `new_prompt` with the original's
///   common leading indent.
fn reindent_prompt(original: &str, new_prompt: &str) -> String {
    let leading_newline = original.starts_with('\n');
    let indent = detect_common_indent(original);
    // Whitespace run that sits between the last `\n` and the closing `"#`,
    // if the original ends with a newline. Usually equals the indent of the
    // closing delimiter. Empty if the original is single-line.
    let trailing_ws = match original.rfind('\n') {
        Some(nl) => {
            let tail = &original[nl + 1..];
            if tail.chars().all(char::is_whitespace) {
                Some(tail.to_string())
            } else {
                None
            }
        }
        None => None,
    };

    let stripped = strip_common_indent(new_prompt);
    let body = indent_non_empty_lines(&stripped, indent);

    let mut out = String::with_capacity(body.len() + indent.len() + 2);
    if leading_newline {
        out.push('\n');
    }
    out.push_str(&body);
    if let Some(tail) = trailing_ws {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&tail);
    }
    out
}

/// Return the longest whitespace prefix shared by every non-empty line.
fn detect_common_indent(text: &str) -> &str {
    let mut common: Option<&str> = None;
    for line in text.lines() {
        if line.chars().all(char::is_whitespace) {
            continue;
        }
        let lead_end = line
            .char_indices()
            .find(|(_, c)| !c.is_whitespace())
            .map(|(i, _)| i)
            .unwrap_or(line.len());
        let lead = &line[..lead_end];
        common = Some(match common {
            None => lead,
            Some(prev) => longest_common_prefix(prev, lead),
        });
    }
    common.unwrap_or("")
}

fn longest_common_prefix<'a>(a: &'a str, b: &'a str) -> &'a str {
    let n = a
        .bytes()
        .zip(b.bytes())
        .take_while(|(x, y)| x == y)
        .count();
    &a[..n]
}

/// Remove the shared leading whitespace from every non-empty line.
fn strip_common_indent(text: &str) -> String {
    let indent = detect_common_indent(text);
    if indent.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if let Some(rest) = line.strip_prefix(indent) {
            out.push_str(rest);
        } else {
            out.push_str(line);
        }
    }
    out
}

/// Prepend `indent` to every non-empty line.
fn indent_non_empty_lines(text: &str, indent: &str) -> String {
    if indent.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + indent.len() * 4);
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if !line.is_empty() {
            out.push_str(indent);
        }
        out.push_str(line);
    }
    out
}

/// Find `function <name>` as a word-boundary match. Returns the byte index
/// of the `f` in `function`.
fn find_function_start(source: &str, name: &str) -> Option<usize> {
    let mut search_from = 0;
    loop {
        let rel = source[search_from..].find("function")?;
        let abs = search_from + rel;

        // Word boundary before.
        if abs > 0 && is_ident_continue(source.as_bytes()[abs - 1]) {
            search_from = abs + "function".len();
            continue;
        }

        let after = abs + "function".len();
        // Must have whitespace after the keyword.
        let ws_end = skip_whitespace(source, after);
        if ws_end == after {
            search_from = after;
            continue;
        }

        let name_end = ws_end + name.len();
        if source.get(ws_end..name_end) == Some(name)
            && source
                .as_bytes()
                .get(name_end)
                .is_none_or(|b| !is_ident_continue(*b))
        {
            return Some(abs);
        }

        search_from = after;
    }
}

/// Find the next occurrence of `keyword` as a standalone word starting at or
/// after `from`. Returns the byte index where the keyword begins.
fn find_keyword_after(source: &str, from: usize, keyword: &str) -> Option<usize> {
    let mut search_from = from;
    loop {
        let rel = source[search_from..].find(keyword)?;
        let abs = search_from + rel;

        let prev_is_bound = abs == 0 || !is_ident_continue(source.as_bytes()[abs - 1]);
        let next = abs + keyword.len();
        let next_is_bound = source
            .as_bytes()
            .get(next)
            .is_none_or(|b| !is_ident_continue(*b));

        if prev_is_bound && next_is_bound {
            return Some(abs);
        }
        search_from = abs + keyword.len();
    }
}

/// Parse the opening of a raw-string literal starting at or after the
/// `prompt` keyword position. Returns `(index_after_opening_quote,
/// hash_count)`. The opening looks like `#+"` (any non-zero number of `#`s
/// followed by a `"`).
fn parse_raw_string_open(source: &str, prompt_kw: usize) -> Option<(usize, usize)> {
    let after_keyword = prompt_kw + "prompt".len();
    let mut i = skip_whitespace(source, after_keyword);

    let bytes = source.as_bytes();
    let hash_start = i;
    while bytes.get(i) == Some(&b'#') {
        i += 1;
    }
    let hash_count = i - hash_start;
    if hash_count == 0 {
        return None;
    }
    if bytes.get(i) != Some(&b'"') {
        return None;
    }
    Some((i + 1, hash_count))
}

/// Find the closing `"###...` of a raw-string literal. The closing is a `"`
/// followed by exactly `hash_count` `#`s. Returns the byte index of the `"`.
fn find_raw_string_close(source: &str, from: usize, hash_count: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            let close = i;
            let mut j = i + 1;
            let mut count = 0;
            while j < bytes.len() && bytes[j] == b'#' {
                count += 1;
                j += 1;
            }
            if count == hash_count {
                return Some(close);
            }
        }
        i += 1;
    }
    None
}

fn skip_whitespace(source: &str, from: usize) -> usize {
    let bytes = source.as_bytes();
    let mut i = from;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidate::{Candidate, CandidateMethod, OptimizableFunction};

    fn sample_candidate(prompt: &str) -> Candidate {
        Candidate {
            id: 0,
            iteration: 0,
            parent_ids: vec![],
            method: CandidateMethod::Initial,
            function: OptimizableFunction {
                function_name: "ExtractSubject".into(),
                prompt_text: prompt.into(),
                classes: vec![],
                enums: vec![],
                function_source: None,
            },
            scores: None,
            rationale: None,
        }
    }

    const RESUME_BAML: &str = r##"class Person { name string }

function ExtractSubject(sentence: string) -> Person? {
  client GPT5
  prompt #"
  Extract the subject from this sentence: {{ sentence }}
"#
}

function OtherFunc(x: string) -> string {
  client GPT5
  prompt #"unchanged"#
}
"##;

    #[test]
    fn rewrites_prompt_preserving_other_functions() {
        let applier = Applier::new(PathBuf::from("/tmp/x"));
        let mut sources = HashMap::new();
        sources.insert(PathBuf::from("r.baml"), RESUME_BAML.to_string());

        let modified = applier
            .generate_modified_files(
                &sample_candidate("IMPROVED prompt for {{ sentence }}."),
                &sources,
            )
            .unwrap();
        let content = &modified[&PathBuf::from("r.baml")];
        assert!(
            content.contains("IMPROVED prompt for {{ sentence }}."),
            "new prompt should be present; got:\n{content}"
        );
        assert!(
            !content.contains("Extract the subject from this sentence"),
            "old prompt should be removed; got:\n{content}"
        );
        assert!(
            content.contains(r##"prompt #"unchanged"#"##),
            "OtherFunc's prompt should not change"
        );
    }

    #[test]
    fn no_change_when_function_absent() {
        let applier = Applier::new(PathBuf::from("/tmp/x"));
        let mut sources = HashMap::new();
        sources.insert(
            PathBuf::from("other.baml"),
            "class Foo { name string }".to_string(),
        );
        let modified = applier
            .generate_modified_files(&sample_candidate("new"), &sources)
            .unwrap();
        assert_eq!(modified, sources);
    }

    #[test]
    fn handles_double_hash_raw_strings() {
        let src = r####"function ExtractSubject(x: string) -> string {
  client GPT5
  prompt ##"contains #" inside"##
}
"####;
        let out = replace_prompt_in_file(src, "ExtractSubject", "replaced").unwrap();
        let expected = r####"prompt ##"replaced"##"####;
        assert!(out.contains(expected), "got:\n{out}");
        assert!(!out.contains("contains #\" inside"));
    }

    #[test]
    fn ignores_function_prefix_collisions() {
        let src = r##"function ExtractSubjectExt(x: string) -> string {
  client GPT5
  prompt #"don't touch me"#
}

function ExtractSubject(x: string) -> string {
  client GPT5
  prompt #"target"#
}
"##;
        let out = replace_prompt_in_file(src, "ExtractSubject", "NEW").unwrap();
        assert!(out.contains(r##"prompt #"don't touch me"#"##));
        assert!(out.contains(r##"prompt #"NEW"#"##));
    }

    #[test]
    fn returns_none_when_prompt_unchanged() {
        let src = r##"function F(x: string) -> string { client GPT5 prompt #"same"# }"##;
        assert!(replace_prompt_in_file(src, "F", "same").is_none());
    }

    #[test]
    fn preserves_indentation_of_original_prompt() {
        let src = r##"function F(x: string) -> string {
  client GPT5
  prompt #"
    Old prompt line.
    Second line.
  "#
}
"##;
        let new_prompt = "New line one.\nNew line two.";
        let out = replace_prompt_in_file(src, "F", new_prompt).unwrap();
        // Both new lines should land with the 4-space indent the original used,
        // and the leading + trailing newlines of the prompt block should be
        // preserved so `"#` stays on its own line.
        assert!(
            out.contains("\n    New line one.\n    New line two.\n  \"#"),
            "got:\n{out}"
        );
    }

    #[test]
    fn normalizes_already_indented_new_prompt() {
        let src = r##"function F(x: string) -> string {
  client GPT5
  prompt #"
    Original.
  "#
}
"##;
        // Reflection model emitted a prompt with its own 2-space indent —
        // shouldn't double up when we re-indent to 4 spaces.
        let new_prompt = "  Line A.\n  Line B.";
        let out = replace_prompt_in_file(src, "F", new_prompt).unwrap();
        assert!(
            out.contains("\n    Line A.\n    Line B.\n  \"#"),
            "got:\n{out}"
        );
        assert!(!out.contains("      Line A"));
    }

    #[test]
    fn single_line_prompt_has_no_indent_to_preserve() {
        let src = r##"function F(x: string) -> string { client GPT5 prompt #"old"# }"##;
        let out = replace_prompt_in_file(src, "F", "new single").unwrap();
        assert!(out.contains(r##"prompt #"new single"#"##), "got:\n{out}");
    }

    fn class_with_description(name: &str, desc: &str) -> ClassDefinition {
        ClassDefinition {
            class_name: name.to_string(),
            description: Some(desc.to_string()),
            fields: vec![],
        }
    }

    #[test]
    fn adds_doc_comment_above_class_without_one() {
        let src = "class Person {\n  name string\n}\n";
        let out = apply_class_description(src, &class_with_description("Person", "A person."))
            .expect("should rewrite");
        assert_eq!(out, "/// A person.\nclass Person {\n  name string\n}\n");
    }

    #[test]
    fn replaces_existing_doc_comment() {
        let src = "/// Outdated.\nclass Person {\n  name string\n}\n";
        let out = apply_class_description(src, &class_with_description("Person", "A person."))
            .expect("should rewrite");
        assert_eq!(out, "/// A person.\nclass Person {\n  name string\n}\n");
    }

    #[test]
    fn preserves_indent_for_nested_class_definitions() {
        let src = "  class Person {\n    name string\n  }\n";
        let out = apply_class_description(src, &class_with_description("Person", "Doc."))
            .expect("should rewrite");
        assert!(out.starts_with("  /// Doc.\n"), "got:\n{out}");
    }

    #[test]
    fn no_change_when_description_matches_existing() {
        let src = "/// A person.\nclass Person {\n  name string\n}\n";
        assert!(
            apply_class_description(src, &class_with_description("Person", "A person.")).is_none()
        );
    }

    #[test]
    fn skips_when_class_absent_from_file() {
        let src = "class Other {\n  x int\n}\n";
        assert!(
            apply_class_description(src, &class_with_description("Person", "Doc.")).is_none()
        );
    }

    #[test]
    fn multiline_description_emits_multiple_doc_lines() {
        let src = "class Person {\n  name string\n}\n";
        let out =
            apply_class_description(src, &class_with_description("Person", "Line one.\nLine two."))
                .expect("should rewrite");
        assert_eq!(
            out,
            "/// Line one.\n/// Line two.\nclass Person {\n  name string\n}\n"
        );
    }

    fn class_with_fields(name: &str, fields: Vec<SchemaFieldDefinition>) -> ClassDefinition {
        ClassDefinition {
            class_name: name.to_string(),
            description: None,
            fields,
        }
    }

    fn field(name: &str, ty: &str, desc: Option<&str>, alias: Option<&str>) -> SchemaFieldDefinition {
        SchemaFieldDefinition {
            field_name: name.to_string(),
            field_type: ty.to_string(),
            description: desc.map(str::to_string),
            alias: alias.map(str::to_string),
        }
    }

    #[test]
    fn adds_description_to_field_without_one() {
        let src = "class Person {\n  name string\n  age int?\n}\n";
        let class = class_with_fields(
            "Person",
            vec![
                field("name", "string", Some("The person's name"), None),
                field("age", "int?", None, None),
            ],
        );
        let out = apply_field_attrs(src, &class).expect("should rewrite");
        assert!(
            out.contains("  name string @description(\"The person's name\")\n"),
            "got:\n{out}"
        );
        assert!(out.contains("  age int?\n"), "age should be untouched:\n{out}");
    }

    #[test]
    fn replaces_existing_description_on_field() {
        let src = "class Person {\n  name string @description(\"old\")\n}\n";
        let class = class_with_fields(
            "Person",
            vec![field("name", "string", Some("new"), None)],
        );
        let out = apply_field_attrs(src, &class).expect("should rewrite");
        assert!(out.contains("@description(\"new\")"), "got:\n{out}");
        assert!(!out.contains("\"old\""), "old value should be gone:\n{out}");
    }

    #[test]
    fn adds_both_description_and_alias() {
        let src = "class Person {\n  name string\n}\n";
        let class = class_with_fields(
            "Person",
            vec![field(
                "name",
                "string",
                Some("The full name"),
                Some("full_name"),
            )],
        );
        let out = apply_field_attrs(src, &class).expect("should rewrite");
        assert!(out.contains("@description(\"The full name\")"), "got:\n{out}");
        assert!(out.contains("@alias(\"full_name\")"), "got:\n{out}");
    }

    #[test]
    fn no_op_when_description_matches_existing() {
        let src = "class Person {\n  name string @description(\"same\")\n}\n";
        let class = class_with_fields(
            "Person",
            vec![field("name", "string", Some("same"), None)],
        );
        assert!(apply_field_attrs(src, &class).is_none());
    }

    #[test]
    fn preserves_other_attributes_on_field() {
        let src = "class Person {\n  age int? @check(AgeNonNeg, {{ this == null or this >= 0 }})\n}\n";
        let class = class_with_fields(
            "Person",
            vec![field("age", "int?", Some("The age"), None)],
        );
        let out = apply_field_attrs(src, &class).expect("should rewrite");
        assert!(out.contains("@check(AgeNonNeg"), "got:\n{out}");
        assert!(out.contains("@description(\"The age\")"), "got:\n{out}");
    }

    #[test]
    fn escapes_quotes_in_attribute_value() {
        let src = "class Person {\n  name string\n}\n";
        let class = class_with_fields(
            "Person",
            vec![field("name", "string", Some("a \"quoted\" name"), None)],
        );
        let out = apply_field_attrs(src, &class).expect("should rewrite");
        assert!(
            out.contains("@description(\"a \\\"quoted\\\" name\")"),
            "got:\n{out}"
        );
    }

    #[test]
    fn preserves_trailing_comment_on_field_line() {
        let src = "class Person {\n  name string // the main name\n}\n";
        let class = class_with_fields(
            "Person",
            vec![field("name", "string", Some("The name"), None)],
        );
        let out = apply_field_attrs(src, &class).expect("should rewrite");
        assert!(
            out.contains("  name string @description(\"The name\") // the main name\n"),
            "got:\n{out}"
        );
    }

    #[test]
    fn only_touches_named_fields() {
        let src = "class Person {\n  name string\n  age int?\n  nickname string?\n}\n";
        let class = class_with_fields(
            "Person",
            vec![field("age", "int?", Some("Age in years"), None)],
        );
        let out = apply_field_attrs(src, &class).expect("should rewrite");
        // Only age gets the new attribute.
        assert!(
            out.contains("  age int? @description(\"Age in years\")\n"),
            "got:\n{out}"
        );
        assert!(
            out.contains("  name string\n"),
            "name should be unchanged:\n{out}"
        );
        assert!(
            out.contains("  nickname string?\n"),
            "nickname should be unchanged:\n{out}"
        );
    }

    #[test]
    fn does_not_touch_other_class() {
        let src = "class Other {\n  name string\n}\n\nclass Person {\n  name string\n}\n";
        let class = class_with_fields(
            "Person",
            vec![field("name", "string", Some("A person's name"), None)],
        );
        let out = apply_field_attrs(src, &class).expect("should rewrite");
        // Only Person's `name` should get the attribute.
        assert!(
            out.contains("class Person {\n  name string @description(\"A person's name\")\n"),
            "got:\n{out}"
        );
        assert!(
            out.contains("class Other {\n  name string\n}"),
            "Other.name should be unchanged:\n{out}"
        );
    }
}
