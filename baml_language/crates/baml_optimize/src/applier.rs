//! Applier - produces modified source files from a candidate.
//!
//! The Applier takes an `OptimizableFunction` from a `Candidate` and the
//! original source files, and produces a new `HashMap<PathBuf, String>` with
//! the candidate's prompt/schema changes applied.
//!
//! The modified source map is the in-memory working format. On-disk candidates
//! are stored as unified-diff patches against a base snapshot (see future
//! checkpoint module), keeping per-candidate artifacts small while any UI can
//! render the diff with standard tooling.
//!
//! Actual span-based prompt rewriting is deferred to a later phase — the
//! current stub returns sources unchanged.

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
    /// prompt/schema edits applied.
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
/// Returns `Some(new_content)` if edits were applied, `None` otherwise.
///
/// TODO: use HIR spans to locate the exact prompt region and rewrite it with
/// `func.prompt_text`. The current stub matches the plan's placeholder — it
/// detects the function by name but doesn't yet perform any edit.
fn apply_prompt_change(source: &str, func: &OptimizableFunction) -> Option<String> {
    let needle = format!("function {}", func.function_name);
    if !source.contains(&needle) {
        return None;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidate::{Candidate, CandidateMethod, OptimizableFunction};

    fn sample_candidate() -> Candidate {
        Candidate {
            id: 0,
            iteration: 0,
            parent_ids: vec![],
            method: CandidateMethod::Initial,
            function: OptimizableFunction {
                function_name: "Foo".into(),
                prompt_text: "new prompt".into(),
                classes: vec![],
                enums: vec![],
                function_source: None,
            },
            scores: None,
            rationale: None,
        }
    }

    #[test]
    fn applier_returns_unchanged_sources_in_stub_mode() {
        let applier = Applier::new(PathBuf::from("/tmp/x"));
        let mut sources = HashMap::new();
        sources.insert(
            PathBuf::from("a.baml"),
            "function Foo(x: string) -> string { client Gpt prompt #\"hi\"# }".to_string(),
        );

        let modified = applier
            .generate_modified_files(&sample_candidate(), &sources)
            .expect("generate_modified_files");

        assert_eq!(modified, sources, "stub must not mutate sources yet");
    }

    #[test]
    fn applier_preserves_unrelated_files() {
        let applier = Applier::new(PathBuf::from("/tmp/x"));
        let mut sources = HashMap::new();
        sources.insert(PathBuf::from("a.baml"), "class Other {}".to_string());
        sources.insert(
            PathBuf::from("b.baml"),
            "function Foo(x: string) -> string { client Gpt prompt #\"hi\"# }".to_string(),
        );

        let modified = applier
            .generate_modified_files(&sample_candidate(), &sources)
            .expect("generate_modified_files");

        assert_eq!(modified.get(&PathBuf::from("a.baml")).unwrap(), "class Other {}");
    }
}
