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

use crate::candidate::{Candidate, OptimizableFunction};

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

/// Apply prompt changes to a single file's source text.
///
/// Returns `Some(new_content)` when the function was found and its prompt
/// block rewritten; `None` otherwise (including when the function is absent
/// from this file or the new prompt matches the existing one).
fn apply_prompt_change(source: &str, func: &OptimizableFunction) -> Option<String> {
    replace_prompt_in_file(source, &func.function_name, &func.prompt_text)
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

    if &source[open_end..close_start] == new_prompt {
        return None;
    }

    let mut out = String::with_capacity(source.len() + new_prompt.len());
    out.push_str(&source[..open_end]);
    out.push_str(new_prompt);
    out.push_str(&source[close_start..]);
    Some(out)
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
}
