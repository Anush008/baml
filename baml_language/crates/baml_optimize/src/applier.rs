//! Applier — produce modified source files from a candidate's
//! [`OptimizableFunction`].
//!
//! The approach is regex/text-based (not IR-driven). We locate `function
//! NAME` in each source file, walk forward to its `prompt` keyword, parse
//! the raw-string delimiters (`#"..."#`, `##"..."##`, etc.), and splice in
//! the candidate's new prompt text between them.
//!
//! This mirrors the fallback path in
//! `engine/baml-runtime/src/optimize/applier.rs::replace_prompt_in_file`.
//! Schema (class/enum) rewriting is intentionally deferred — modern GEPA
//! prompts rarely require schema changes, and doing it reliably needs IR
//! span access we don't have yet.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;

use crate::candidate::{Candidate, ClassDefinition, OptimizableFunction};

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
    }

    if changed { Some(current) } else { None }
}

/// If `class.description` is set, add or replace a `///` doc-comment block
/// immediately above the `class <ClassName>` line in `source`.
///
/// Per-field attributes (`@description`, `@alias`) are deferred — handling
/// them reliably needs IR span access we don't have yet.
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
}
